//! ECS task-role credentials: `GET /_fakecloud/ecs/creds/{task_id}` (the URL
//! injected as `AWS_CONTAINER_CREDENTIALS_FULL_URI`) vends the task role's
//! session, named after the task, that verifies under `--verify-sigv4` and
//! acts as the role under `--iam strict`; revoked once the task stops.
//! RegisterTaskDefinition refuses roles ECS tasks cannot assume.

mod helpers;

use std::time::Duration;

use aws_sdk_ecs::types::ContainerDefinition;
use helpers::TestServer;

const ECS_TASKS_TRUST: &str = r#"{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Principal":{"Service":"ecs-tasks.amazonaws.com"},"Action":"sts:AssumeRole"}]}"#;
const EC2_TRUST: &str = r#"{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Principal":{"Service":"ec2.amazonaws.com"},"Action":"sts:AssumeRole"}]}"#;

fn docker_available() -> bool {
    std::process::Command::new("docker")
        .arg("info")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn require_docker_or_skip(test: &str) -> bool {
    if docker_available() {
        return true;
    }
    if std::env::var("CI").is_ok() {
        panic!("docker is required for {test} in CI");
    }
    eprintln!("skipping {test}: docker is not available");
    false
}

async fn wait_status(ecs: &aws_sdk_ecs::Client, cluster: &str, arn: &str, want: &str) {
    for _ in 0..240 {
        let desc = ecs
            .describe_tasks()
            .cluster(cluster)
            .tasks(arn)
            .send()
            .await
            .unwrap();
        let status = desc.tasks()[0]
            .last_status()
            .unwrap_or_default()
            .to_string();
        if status == want {
            return;
        }
        assert!(
            want != "RUNNING" || status != "STOPPED",
            "task {arn} stopped before running: {:?}",
            desc.tasks()[0].stopped_reason()
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    panic!("task {arn} never reached {want}");
}

async fn fetch_creds(server: &TestServer, task_id: &str) -> (u16, serde_json::Value) {
    let resp = reqwest::Client::new()
        .get(format!(
            "{}/_fakecloud/ecs/creds/{task_id}",
            server.endpoint()
        ))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap())
}

fn sdk_config(server: &TestServer, creds: &serde_json::Value) -> aws_config::SdkConfig {
    let credentials = aws_credential_types::Credentials::new(
        creds["AccessKeyId"].as_str().unwrap(),
        creds["SecretAccessKey"].as_str().unwrap(),
        Some(creds["Token"].as_str().unwrap().to_string()),
        None,
        "ecs-task",
    );
    aws_config::SdkConfig::builder()
        .behavior_version(aws_config::BehaviorVersion::latest())
        .endpoint_url(server.endpoint())
        .region(aws_config::Region::new("us-east-1"))
        .credentials_provider(
            aws_credential_types::provider::SharedCredentialsProvider::new(credentials),
        )
        .build()
}

/// SDK config signed with the reserved root-bypass credentials, which skip
/// SigV4 verification and IAM enforcement.
async fn root_config(server: &TestServer) -> aws_config::SdkConfig {
    aws_config::defaults(aws_config::BehaviorVersion::latest())
        .endpoint_url(server.endpoint())
        .region(aws_config::Region::new("us-east-1"))
        .credentials_provider(aws_credential_types::Credentials::new(
            "test", "test", None, None, "test",
        ))
        .load()
        .await
}

fn assert_not_found(status: u16, body: &serde_json::Value) {
    assert_eq!(status, 400, "{body}");
    assert_eq!(
        body,
        &serde_json::json!({
            "code": "InvalidIdInRequest",
            "message": "CredentialsV2Request: Credentials not found",
            "HTTPErrorCode": 400,
        })
    );
}

/// A running task's credentials are its role's session named after the task:
/// they verify under --verify-sigv4, are evaluated as the role under
/// --iam strict, are reachable from inside the container, and stop working
/// once the task stops.
#[tokio::test]
async fn running_task_gets_its_role_session_until_it_stops() {
    if !require_docker_or_skip("running_task_gets_its_role_session_until_it_stops") {
        return;
    }
    let server = TestServer::start_with_env(&[
        ("FAKECLOUD_VERIFY_SIGV4", "true"),
        ("FAKECLOUD_IAM", "strict"),
    ])
    .await;
    let root = root_config(&server).await;
    let iam = aws_sdk_iam::Client::new(&root);
    let ecs = aws_sdk_ecs::Client::new(&root);

    let role = iam
        .create_role()
        .role_name("app-task-role")
        .assume_role_policy_document(ECS_TASKS_TRUST)
        .send()
        .await
        .unwrap();
    let role_arn = role.role().unwrap().arn().to_string();
    iam.put_role_policy()
        .role_name("app-task-role")
        .policy_name("list-queues")
        .policy_document(
            r#"{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Action":["sqs:ListQueues","sts:GetCallerIdentity"],"Resource":"*"}]}"#,
        )
        .send()
        .await
        .unwrap();

    ecs.create_cluster()
        .cluster_name("creds-cluster")
        .send()
        .await
        .unwrap();
    ecs.register_task_definition()
        .family("creds-family")
        .task_role_arn(&role_arn)
        .container_definitions(
            ContainerDefinition::builder()
                .name("app")
                .image("public.ecr.aws/docker/library/alpine:3.20")
                .essential(true)
                .command("sh")
                .command("-c")
                // Fetch the credentials the way an SDK in the task would, then
                // keep running so the host can use them while the task lives.
                .command(
                    "wget -qO- \"$AWS_CONTAINER_CREDENTIALS_FULL_URI\" | grep -o '\"RoleArn\":\"[^\"]*\"'; sleep 300",
                )
                .build(),
        )
        .send()
        .await
        .unwrap();

    let run = ecs
        .run_task()
        .cluster("creds-cluster")
        .task_definition("creds-family")
        .send()
        .await
        .unwrap();
    let task_arn = run.tasks()[0].task_arn().unwrap().to_string();
    let task_id = task_arn.rsplit('/').next().unwrap().to_string();
    wait_status(&ecs, "creds-cluster", &task_arn, "RUNNING").await;

    let (status, creds) = fetch_creds(&server, &task_id).await;
    assert_eq!(status, 200, "{creds}");
    assert_eq!(creds["RoleArn"].as_str(), Some(role_arn.as_str()));
    for field in ["AccessKeyId", "SecretAccessKey", "Token", "Expiration"] {
        assert!(creds[field].is_string(), "missing {field}: {creds}");
    }
    // Refetching within the validity window hands back the same session.
    let (_, again) = fetch_creds(&server, &task_id).await;
    assert_eq!(again["AccessKeyId"], creds["AccessKeyId"]);

    // The container reached the endpoint through the injected URI.
    let expected = format!("\"RoleArn\":\"{role_arn}\"");
    let mut in_container = false;
    for _ in 0..60 {
        let logs: serde_json::Value = reqwest::Client::new()
            .get(format!(
                "{}/_fakecloud/ecs/tasks/{task_id}/logs",
                server.endpoint()
            ))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if logs["logs"]
            .as_str()
            .unwrap_or_default()
            .contains(&expected)
        {
            in_container = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    // Captured logs are only guaranteed once the container exits, so the
    // in-container fetch is checked again after the stop below.

    let task_config = sdk_config(&server, &creds);
    let identity = aws_sdk_sts::Client::new(&task_config)
        .get_caller_identity()
        .send()
        .await
        .expect("task credentials verify under --verify-sigv4");
    assert_eq!(
        identity.arn(),
        Some(format!("arn:aws:sts::123456789012:assumed-role/app-task-role/{task_id}").as_str())
    );

    // Under --iam strict the session acts as the role: its policy allows
    // ListQueues (and GetCallerIdentity) and nothing else.
    let sqs = aws_sdk_sqs::Client::new(&task_config);
    sqs.list_queues()
        .send()
        .await
        .expect("the task role allows sqs:ListQueues");
    let denied = sqs
        .create_queue()
        .queue_name("not-allowed")
        .send()
        .await
        .expect_err("the task role does not allow sqs:CreateQueue");
    let code = denied
        .into_service_error()
        .meta()
        .code()
        .map(str::to_string);
    assert!(
        matches!(
            code.as_deref(),
            Some("AccessDenied") | Some("AccessDeniedException")
        ),
        "{code:?}"
    );

    ecs.stop_task()
        .cluster("creds-cluster")
        .task(&task_arn)
        .send()
        .await
        .unwrap();
    wait_status(&ecs, "creds-cluster", &task_arn, "STOPPED").await;

    if !in_container {
        let logs: serde_json::Value = reqwest::Client::new()
            .get(format!(
                "{}/_fakecloud/ecs/tasks/{task_id}/logs",
                server.endpoint()
            ))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(
            logs["logs"]
                .as_str()
                .unwrap_or_default()
                .contains(&expected),
            "container did not fetch its credentials: {logs}"
        );
    }

    // The endpoint refuses the stopped task, and the session it handed out
    // no longer authenticates.
    let (status, body) = fetch_creds(&server, &task_id).await;
    assert_not_found(status, &body);
    let mut revoked = false;
    for _ in 0..20 {
        if aws_sdk_sts::Client::new(&task_config)
            .get_caller_identity()
            .send()
            .await
            .is_err()
        {
            revoked = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert!(revoked, "stopped task's credentials still authenticate");
}

/// A task without a task role has no credentials, and neither does an ID no
/// task has: both answered like the ECS agent.
#[tokio::test]
async fn task_without_role_and_unknown_id_get_the_agent_error() {
    let server = TestServer::start().await;
    let (status, body) = fetch_creds(&server, "0123456789abcdef0123456789abcdef").await;
    assert_not_found(status, &body);

    if !require_docker_or_skip("task_without_role_and_unknown_id_get_the_agent_error") {
        return;
    }
    let ecs = server.ecs_client().await;
    ecs.create_cluster()
        .cluster_name("norole-cluster")
        .send()
        .await
        .unwrap();
    ecs.register_task_definition()
        .family("norole-family")
        .container_definitions(
            ContainerDefinition::builder()
                .name("app")
                .image("public.ecr.aws/docker/library/alpine:3.20")
                .essential(true)
                .command("sh")
                .command("-c")
                .command("echo FULL_URI=[$AWS_CONTAINER_CREDENTIALS_FULL_URI]; sleep 300")
                .build(),
        )
        .send()
        .await
        .unwrap();
    let run = ecs
        .run_task()
        .cluster("norole-cluster")
        .task_definition("norole-family")
        .send()
        .await
        .unwrap();
    let task_arn = run.tasks()[0].task_arn().unwrap().to_string();
    let task_id = task_arn.rsplit('/').next().unwrap().to_string();
    wait_status(&ecs, "norole-cluster", &task_arn, "RUNNING").await;

    let (status, body) = fetch_creds(&server, &task_id).await;
    assert_not_found(status, &body);

    ecs.stop_task()
        .cluster("norole-cluster")
        .task(&task_arn)
        .send()
        .await
        .unwrap();
    wait_status(&ecs, "norole-cluster", &task_arn, "STOPPED").await;
    let logs: serde_json::Value = reqwest::Client::new()
        .get(format!(
            "{}/_fakecloud/ecs/tasks/{task_id}/logs",
            server.endpoint()
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    // As on AWS, a task with no role gets no credentials URI at all.
    assert!(
        logs["logs"]
            .as_str()
            .unwrap_or_default()
            .contains("FULL_URI=[]"),
        "{logs}"
    );
}

/// RegisterTaskDefinition refuses a task or execution role ECS tasks cannot
/// assume (trust policy without `ecs-tasks.amazonaws.com`) and, under IAM
/// enforcement, another account's role, with ECS's ClientException.
#[tokio::test]
async fn register_task_definition_refuses_roles_ecs_cannot_assume() {
    let server = TestServer::start_with_env(&[("FAKECLOUD_IAM", "strict")]).await;
    let iam = server.iam_client().await;
    let ecs = server.ecs_client().await;

    let untrusted = iam
        .create_role()
        .role_name("ec2-only")
        .assume_role_policy_document(EC2_TRUST)
        .send()
        .await
        .unwrap()
        .role()
        .unwrap()
        .arn()
        .to_string();
    let trusted = iam
        .create_role()
        .role_name("ecs-ok")
        .assume_role_policy_document(ECS_TASKS_TRUST)
        .send()
        .await
        .unwrap()
        .role()
        .unwrap()
        .arn()
        .to_string();

    let register = |task_role: String, exec_role: String| {
        ecs.register_task_definition()
            .family("role-checked")
            .task_role_arn(task_role)
            .execution_role_arn(exec_role)
            .container_definitions(
                ContainerDefinition::builder()
                    .name("app")
                    .image("public.ecr.aws/docker/library/alpine:3.20")
                    .build(),
            )
            .send()
    };

    let foreign = "arn:aws:iam::999999999999:role/elsewhere".to_string();
    for (task_role, exec_role, refused) in [
        (untrusted.clone(), trusted.clone(), &untrusted),
        (trusted.clone(), untrusted.clone(), &untrusted),
        (foreign.clone(), trusted.clone(), &foreign),
    ] {
        let err = register(task_role, exec_role)
            .await
            .expect_err("role ECS cannot assume must be refused");
        let status = err.raw_response().map(|r| r.status().as_u16());
        let err = err.into_service_error();
        assert!(err.is_client_exception(), "{err:?}");
        assert_eq!(
            err.meta().message(),
            Some(
                format!(
                    "ECS was unable to assume the role '{refused}' that was provided for this task. \
                     Please verify that the role being passed has the proper trust relationship and \
                     permissions and that your IAM user has permissions to pass this role."
                )
                .as_str()
            )
        );
        assert_eq!(status, Some(400));
    }

    register(trusted.clone(), trusted)
        .await
        .expect("roles that trust ecs-tasks.amazonaws.com register");
}
