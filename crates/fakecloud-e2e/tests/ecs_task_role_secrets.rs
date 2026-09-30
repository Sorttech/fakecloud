//! ECS task-role credentials + secrets injection.
//!
//! - Tasks with a `taskRoleArn` get `AWS_CONTAINER_CREDENTIALS_FULL_URI`
//!   injected and a dedicated HTTP endpoint on the main fakecloud
//!   server vends IMDS-format credentials.
//! - Tasks with `containerDefinitions[].secrets[]` entries get the
//!   referenced SecretsManager secrets / SSM parameters resolved
//!   synchronously and injected as env vars.
//!
//! Docker-gated the same way the Lambda invoke tests are.

mod helpers;

use std::time::Duration;

use aws_sdk_ecs::types::{ContainerDefinition, Secret};
use aws_sdk_ssm::types::ParameterType;
use helpers::TestServer;

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

async fn wait_stopped(ecs: &aws_sdk_ecs::Client, cluster: &str, arn: &str) {
    for _ in 0..120 {
        let desc = ecs
            .describe_tasks()
            .cluster(cluster)
            .tasks(arn)
            .send()
            .await
            .unwrap();
        if desc.tasks()[0].last_status() == Some("STOPPED") {
            return;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    panic!("task {arn} never reached STOPPED");
}

async fn task_logs(endpoint: &str, task_id: &str) -> String {
    reqwest::Client::new()
        .get(format!("{endpoint}/_fakecloud/ecs/tasks/{task_id}/logs"))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap()["logs"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

/// Creating a task with a `taskRoleArn` causes the runtime to inject
/// `AWS_CONTAINER_CREDENTIALS_FULL_URI`. Fetching that URL from inside
/// the container returns IMDS-format credentials.
#[tokio::test]
async fn ecs_task_role_credentials_are_served() {
    if !require_docker_or_skip("ecs_task_role_credentials_are_served") {
        return;
    }
    let server = TestServer::start().await;
    let ecs = server.ecs_client().await;

    ecs.create_cluster()
        .cluster_name("task-role-cluster")
        .send()
        .await
        .unwrap();
    ecs.register_task_definition()
        .family("task-role-family")
        .task_role_arn("arn:aws:iam::123456789012:role/app-task-role")
        .container_definitions(
            ContainerDefinition::builder()
                .name("probe")
                .image("public.ecr.aws/docker/library/alpine:3.20")
                .essential(true)
                .command("sh")
                .command("-c")
                // Print the creds env + fetch them via apk + wget. No
                // wget on alpine:3.20 default — use nc/getent? Simpler:
                // just print the env var so we can see injection worked.
                // A curl/wget pull is the integration check; we assert
                // via direct HTTP from the host instead.
                .command("echo FULL_URI=$AWS_CONTAINER_CREDENTIALS_FULL_URI")
                .build(),
        )
        .send()
        .await
        .unwrap();

    let run = ecs
        .run_task()
        .cluster("task-role-cluster")
        .task_definition("task-role-family")
        .send()
        .await
        .unwrap();
    let arn = run.tasks()[0].task_arn().unwrap().to_string();
    wait_stopped(&ecs, "task-role-cluster", &arn).await;

    let task_id = arn.rsplit('/').next().unwrap();
    let logs = task_logs(server.endpoint(), task_id).await;
    assert!(
        logs.contains("FULL_URI=http://host.docker.internal:")
            && logs.contains(&format!("/_fakecloud/ecs/creds/{task_id}")),
        "env var not injected; logs: {logs}"
    );

    // Direct probe of the credential endpoint — same JSON the AWS SDK's
    // ContainerCredentialsProvider would fetch from inside the container.
    let creds: serde_json::Value = reqwest::Client::new()
        .get(format!(
            "{}/_fakecloud/ecs/creds/{task_id}",
            server.endpoint()
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        creds["AccessKeyId"]
            .as_str()
            .unwrap_or("")
            .starts_with("ASIA"),
        "AccessKeyId shape unexpected: {creds}"
    );
    assert_eq!(
        creds["RoleArn"].as_str(),
        Some("arn:aws:iam::123456789012:role/app-task-role"),
        "RoleArn mismatch: {creds}"
    );
    assert!(creds["Token"].as_str().is_some(), "missing Token: {creds}");
    assert!(
        creds["Expiration"].as_str().is_some(),
        "missing Expiration: {creds}"
    );
}

/// Container `secrets[]` entries resolve against SecretsManager.
#[tokio::test]
async fn ecs_task_resolves_secretsmanager_secret() {
    if !require_docker_or_skip("ecs_task_resolves_secretsmanager_secret") {
        return;
    }
    let server = TestServer::start().await;
    let secrets = server.secretsmanager_client().await;
    let ecs = server.ecs_client().await;

    let created = secrets
        .create_secret()
        .name("ecs/db/password")
        .secret_string("super-secret-value")
        .send()
        .await
        .unwrap();
    let secret_arn = created.arn().unwrap().to_string();

    ecs.create_cluster()
        .cluster_name("secrets-cluster")
        .send()
        .await
        .unwrap();
    ecs.register_task_definition()
        .family("secrets-family")
        .container_definitions(
            ContainerDefinition::builder()
                .name("reader")
                .image("public.ecr.aws/docker/library/alpine:3.20")
                .essential(true)
                .secrets(
                    Secret::builder()
                        .name("DB_PASSWORD")
                        .value_from(&secret_arn)
                        .build()
                        .unwrap(),
                )
                .command("sh")
                .command("-c")
                .command("echo DB=$DB_PASSWORD")
                .build(),
        )
        .send()
        .await
        .unwrap();

    let run = ecs
        .run_task()
        .cluster("secrets-cluster")
        .task_definition("secrets-family")
        .send()
        .await
        .unwrap();
    let arn = run.tasks()[0].task_arn().unwrap().to_string();
    wait_stopped(&ecs, "secrets-cluster", &arn).await;
    let task_id = arn.rsplit('/').next().unwrap();
    let logs = task_logs(server.endpoint(), task_id).await;
    assert!(
        logs.contains("DB=super-secret-value"),
        "secret not injected: {logs}"
    );
}

/// Container `secrets[]` entries resolve against SSM Parameter Store.
#[tokio::test]
async fn ecs_task_resolves_ssm_parameter() {
    if !require_docker_or_skip("ecs_task_resolves_ssm_parameter") {
        return;
    }
    let server = TestServer::start().await;
    let ssm = server.ssm_client().await;
    let ecs = server.ecs_client().await;

    ssm.put_parameter()
        .name("/ecs/app/api-key")
        .value("param-store-value")
        .r#type(ParameterType::String)
        .send()
        .await
        .unwrap();
    let param_arn = "arn:aws:ssm:us-east-1:123456789012:parameter/ecs/app/api-key";

    ecs.create_cluster()
        .cluster_name("ssm-cluster")
        .send()
        .await
        .unwrap();
    ecs.register_task_definition()
        .family("ssm-family")
        .container_definitions(
            ContainerDefinition::builder()
                .name("reader")
                .image("public.ecr.aws/docker/library/alpine:3.20")
                .essential(true)
                .secrets(
                    Secret::builder()
                        .name("API_KEY")
                        .value_from(param_arn)
                        .build()
                        .unwrap(),
                )
                .command("sh")
                .command("-c")
                .command("echo KEY=$API_KEY")
                .build(),
        )
        .send()
        .await
        .unwrap();

    let run = ecs
        .run_task()
        .cluster("ssm-cluster")
        .task_definition("ssm-family")
        .send()
        .await
        .unwrap();
    let arn = run.tasks()[0].task_arn().unwrap().to_string();
    wait_stopped(&ecs, "ssm-cluster", &arn).await;
    let task_id = arn.rsplit('/').next().unwrap();
    let logs = task_logs(server.endpoint(), task_id).await;
    assert!(
        logs.contains("KEY=param-store-value"),
        "parameter not injected: {logs}"
    );
}

/// A missing secret ARN should fail the task fast with TaskFailedToStart
/// — matches real ECS's "failed to retrieve secret" behaviour.
#[tokio::test]
async fn ecs_task_with_missing_secret_fails_fast() {
    if !require_docker_or_skip("ecs_task_with_missing_secret_fails_fast") {
        return;
    }
    let server = TestServer::start().await;
    let ecs = server.ecs_client().await;

    ecs.create_cluster()
        .cluster_name("missing-secret-cluster")
        .send()
        .await
        .unwrap();
    ecs.register_task_definition()
        .family("missing-secret-family")
        .container_definitions(
            ContainerDefinition::builder()
                .name("reader")
                .image("public.ecr.aws/docker/library/alpine:3.20")
                .essential(true)
                .secrets(
                    Secret::builder()
                        .name("X")
                        .value_from(
                            "arn:aws:secretsmanager:us-east-1:123456789012:secret:does-not-exist-AbCdEf",
                        )
                        .build()
                        .unwrap(),
                )
                .command("true")
                .build(),
        )
        .send()
        .await
        .unwrap();

    let run = ecs
        .run_task()
        .cluster("missing-secret-cluster")
        .task_definition("missing-secret-family")
        .send()
        .await
        .unwrap();
    let arn = run.tasks()[0].task_arn().unwrap().to_string();

    let mut stop_code: Option<String> = None;
    for _ in 0..60 {
        let desc = ecs
            .describe_tasks()
            .cluster("missing-secret-cluster")
            .tasks(&arn)
            .send()
            .await
            .unwrap();
        let t = &desc.tasks()[0];
        if t.last_status() == Some("STOPPED") {
            stop_code = t.stop_code().map(|c| c.as_str().to_string());
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert_eq!(
        stop_code.as_deref(),
        Some("TaskFailedToStart"),
        "expected TaskFailedToStart; got {stop_code:?}"
    );
}

/// `valueFrom` selectors: a json-key out of a JSON secret, a previous
/// version by staging label, a partial ARN, and an SSM SecureString, all
/// injected as their resolved (decrypted) values.
#[tokio::test]
async fn ecs_task_resolves_secret_selectors_and_secure_string() {
    if !require_docker_or_skip("ecs_task_resolves_secret_selectors_and_secure_string") {
        return;
    }
    let server = TestServer::start().await;
    let secrets = server.secretsmanager_client().await;
    let ssm = server.ssm_client().await;
    let ecs = server.ecs_client().await;

    let arn = secrets
        .create_secret()
        .name("ecs/db/creds")
        .secret_string(r#"{"username":"admin","password":"first","port":5432}"#)
        .send()
        .await
        .unwrap()
        .arn()
        .unwrap()
        .to_string();
    secrets
        .put_secret_value()
        .secret_id(&arn)
        .secret_string(r#"{"username":"admin","password":"second","port":5432}"#)
        .send()
        .await
        .unwrap();
    // A sibling whose name extends this one must not shadow it.
    secrets
        .create_secret()
        .name("ecs/db/creds-old")
        .secret_string("wrong")
        .send()
        .await
        .unwrap();
    ssm.put_parameter()
        .name("/ecs/app/token")
        .value("secure-token")
        .r#type(ParameterType::SecureString)
        .send()
        .await
        .unwrap();
    let partial_arn = &arn[..arn.len() - 7];

    let secret = |name: &str, value_from: String| {
        Secret::builder()
            .name(name)
            .value_from(value_from)
            .build()
            .unwrap()
    };
    ecs.create_cluster()
        .cluster_name("selectors-cluster")
        .send()
        .await
        .unwrap();
    ecs.register_task_definition()
        .family("selectors-family")
        .container_definitions(
            ContainerDefinition::builder()
                .name("reader")
                .image("public.ecr.aws/docker/library/alpine:3.20")
                .essential(true)
                .secrets(secret("CUR_PW", format!("{arn}:password::")))
                .secrets(secret("PREV_PW", format!("{arn}:password:AWSPREVIOUS:")))
                .secrets(secret("PORT", format!("{arn}:port::")))
                .secrets(secret("WHOLE", partial_arn.to_string()))
                .secrets(secret("TOKEN", "/ecs/app/token".to_string()))
                .command("sh")
                .command("-c")
                .command(
                    "echo CUR=$CUR_PW PREV=$PREV_PW PORT=$PORT TOKEN=$TOKEN; echo WHOLE=$WHOLE",
                )
                .build(),
        )
        .send()
        .await
        .unwrap();

    let run = ecs
        .run_task()
        .cluster("selectors-cluster")
        .task_definition("selectors-family")
        .send()
        .await
        .unwrap();
    let task_arn = run.tasks()[0].task_arn().unwrap().to_string();
    wait_stopped(&ecs, "selectors-cluster", &task_arn).await;
    let task_id = task_arn.rsplit('/').next().unwrap();
    let logs = task_logs(server.endpoint(), task_id).await;
    assert!(
        logs.contains("CUR=second PREV=first PORT=5432 TOKEN=secure-token"),
        "selectors not resolved: {logs}"
    );
    assert!(
        logs.contains(r#"WHOLE={"username":"admin","password":"second","port":5432}"#),
        "partial ARN not resolved to its own secret: {logs}"
    );
}

/// A json-key the secret lacks stops the task with ECS's
/// ResourceInitializationError reason.
#[tokio::test]
async fn ecs_task_with_missing_json_key_fails_with_resource_initialization_error() {
    if !require_docker_or_skip(
        "ecs_task_with_missing_json_key_fails_with_resource_initialization_error",
    ) {
        return;
    }
    let server = TestServer::start().await;
    let secrets = server.secretsmanager_client().await;
    let ecs = server.ecs_client().await;

    let arn = secrets
        .create_secret()
        .name("ecs/no-key")
        .secret_string(r#"{"username":"admin"}"#)
        .send()
        .await
        .unwrap()
        .arn()
        .unwrap()
        .to_string();
    ecs.create_cluster()
        .cluster_name("json-key-cluster")
        .send()
        .await
        .unwrap();
    ecs.register_task_definition()
        .family("json-key-family")
        .container_definitions(
            ContainerDefinition::builder()
                .name("reader")
                .image("public.ecr.aws/docker/library/alpine:3.20")
                .essential(true)
                .secrets(
                    Secret::builder()
                        .name("PW")
                        .value_from(format!("{arn}:password::"))
                        .build()
                        .unwrap(),
                )
                .command("true")
                .build(),
        )
        .send()
        .await
        .unwrap();
    let run = ecs
        .run_task()
        .cluster("json-key-cluster")
        .task_definition("json-key-family")
        .send()
        .await
        .unwrap();
    let task_arn = run.tasks()[0].task_arn().unwrap().to_string();
    wait_stopped(&ecs, "json-key-cluster", &task_arn).await;
    let desc = ecs
        .describe_tasks()
        .cluster("json-key-cluster")
        .tasks(&task_arn)
        .send()
        .await
        .unwrap();
    let task = &desc.tasks()[0];
    assert_eq!(
        task.stop_code().map(|c| c.as_str()),
        Some("TaskFailedToStart")
    );
    assert_eq!(
        task.stopped_reason(),
        Some(
            "ResourceInitializationError: unable to pull secrets or registry auth: execution \
             resource retrieval failed: unable to retrieve secret from asm: retrieved secret \
             from Secrets Manager did not contain json key password"
        )
    );
}
