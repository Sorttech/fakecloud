//! Verify ECS-side enforcement gates that until O15 only stored fields:
//!
//! 1. ExecuteCommand must reject when the target task did not opt in
//!    via `enableExecuteCommand` at RunTask / Service-create time.
//! 2. `propagateTags=TASK_DEFINITION` on a service must copy the task
//!    definition's tags onto every task the service spawns; clients
//!    can read them back via `ListTagsForResource`.
//! 3. `UpdateService` scale-down must skip tasks marked with
//!    `protectFromScaleIn` via `UpdateTaskProtection` and pick an
//!    unprotected task instead.

mod helpers;

use aws_sdk_ecs::types::{ContainerDefinition, PropagateTags, Tag, TaskField};
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

async fn bootstrap(client: &aws_sdk_ecs::Client, cluster: &str, family: &str, tags: Vec<Tag>) {
    client
        .create_cluster()
        .cluster_name(cluster)
        .send()
        .await
        .unwrap();
    let mut td = client
        .register_task_definition()
        .family(family)
        .container_definitions(
            ContainerDefinition::builder()
                .name("app")
                .image("public.ecr.aws/docker/library/alpine:latest")
                .essential(true)
                .build(),
        );
    for t in tags {
        td = td.tags(t);
    }
    td.send().await.unwrap();
}

#[tokio::test]
async fn execute_command_rejected_when_service_did_not_enable_it() {
    let server = TestServer::start().await;
    let client = server.ecs_client().await;
    bootstrap(&client, "exec-off", "exec-off-td", vec![]).await;

    client
        .create_service()
        .cluster("exec-off")
        .service_name("web")
        .task_definition("exec-off-td")
        .desired_count(1)
        // enable_execute_command intentionally omitted (default = false).
        .send()
        .await
        .expect("create_service");

    let tasks = client
        .list_tasks()
        .cluster("exec-off")
        .send()
        .await
        .expect("list_tasks");
    let task_arn = tasks.task_arns().first().expect("one task").clone();

    let err = client
        .execute_command()
        .cluster("exec-off")
        .task(&task_arn)
        .command("/bin/sh -c 'echo hi'")
        .interactive(false)
        .send()
        .await
        .expect_err("ExecuteCommand should be rejected");

    let msg = format!("{err:?}");
    assert!(
        msg.contains("InvalidParameterException")
            || msg.contains("execute command was not enabled"),
        "unexpected error: {msg}"
    );
}

#[tokio::test]
async fn execute_command_allowed_when_service_enabled_it() {
    let server = TestServer::start().await;
    let client = server.ecs_client().await;
    bootstrap(&client, "exec-on", "exec-on-td", vec![]).await;

    client
        .create_service()
        .cluster("exec-on")
        .service_name("web")
        .task_definition("exec-on-td")
        .desired_count(1)
        .enable_execute_command(true)
        .send()
        .await
        .expect("create_service");

    let tasks = client
        .list_tasks()
        .cluster("exec-on")
        .send()
        .await
        .expect("list_tasks");
    let task_arn = tasks.task_arns().first().expect("one task").clone();

    // ExecuteCommand returns a session shape. We don't assert the
    // session contents (docker exec may or may not be available in
    // CI), only that the gate didn't reject the call.
    let resp = client
        .execute_command()
        .cluster("exec-on")
        .task(&task_arn)
        .command("/bin/sh -c 'true'")
        .interactive(false)
        .send()
        .await;
    // In CI without docker, the runtime may surface a docker-exec
    // failure; the gate itself must not return InvalidParameterException.
    if let Err(err) = resp {
        let msg = format!("{err:?}");
        assert!(
            !msg.contains("InvalidParameterException"),
            "ExecuteCommand should not be gated when service enabled it: {msg}"
        );
    }

    // Surface check: DescribeTasks should report enableExecuteCommand=true.
    let described = client
        .describe_tasks()
        .cluster("exec-on")
        .tasks(&task_arn)
        .send()
        .await
        .expect("describe_tasks");
    let task = described.tasks().first().expect("task present");
    assert!(task.enable_execute_command());
}

#[tokio::test]
async fn propagate_tags_task_definition_copies_td_tags_onto_tasks() {
    let server = TestServer::start().await;
    let client = server.ecs_client().await;
    let td_tags = vec![
        Tag::builder().key("Project").value("alpha").build(),
        Tag::builder().key("Env").value("test").build(),
    ];
    bootstrap(&client, "tag-cluster", "tag-td", td_tags.clone()).await;

    client
        .create_service()
        .cluster("tag-cluster")
        .service_name("web")
        .task_definition("tag-td")
        .desired_count(1)
        .propagate_tags(PropagateTags::TaskDefinition)
        .send()
        .await
        .expect("create_service");

    let listed = client
        .list_tasks()
        .cluster("tag-cluster")
        .send()
        .await
        .expect("list_tasks");
    let task_arn = listed.task_arns().first().expect("one task").clone();

    let described = client
        .describe_tasks()
        .cluster("tag-cluster")
        .tasks(&task_arn)
        .include(TaskField::Tags)
        .send()
        .await
        .expect("describe_tasks");
    let task = described.tasks().first().expect("task present");
    let tag_keys: Vec<_> = task.tags().iter().filter_map(|t| t.key()).collect();
    assert!(
        tag_keys.contains(&"Project") && tag_keys.contains(&"Env"),
        "propagateTags=TASK_DEFINITION should copy TD tags onto tasks; got {tag_keys:?}"
    );

    // ListTagsForResource on the task ARN must surface them too.
    let lt = client
        .list_tags_for_resource()
        .resource_arn(&task_arn)
        .send()
        .await
        .expect("list_tags_for_resource");
    let lt_keys: Vec<_> = lt.tags().iter().filter_map(|t| t.key()).collect();
    assert!(
        lt_keys.contains(&"Project") && lt_keys.contains(&"Env"),
        "ListTagsForResource on the task should reflect propagated TD tags; got {lt_keys:?}"
    );
}

#[tokio::test]
async fn propagate_tags_service_copies_service_tags_onto_tasks() {
    let server = TestServer::start().await;
    let client = server.ecs_client().await;
    bootstrap(&client, "svc-tag-cluster", "svc-tag-td", vec![]).await;

    client
        .create_service()
        .cluster("svc-tag-cluster")
        .service_name("web")
        .task_definition("svc-tag-td")
        .desired_count(1)
        .propagate_tags(PropagateTags::Service)
        .tags(Tag::builder().key("Owner").value("platform").build())
        .send()
        .await
        .expect("create_service");

    let listed = client
        .list_tasks()
        .cluster("svc-tag-cluster")
        .send()
        .await
        .expect("list_tasks");
    let task_arn = listed.task_arns().first().expect("one task").clone();

    let described = client
        .describe_tasks()
        .cluster("svc-tag-cluster")
        .tasks(&task_arn)
        .include(TaskField::Tags)
        .send()
        .await
        .expect("describe_tasks");
    let task = described.tasks().first().expect("task present");
    let tag_keys: Vec<_> = task.tags().iter().filter_map(|t| t.key()).collect();
    assert!(
        tag_keys.contains(&"Owner"),
        "propagateTags=SERVICE should copy Service tags onto tasks; got {tag_keys:?}"
    );
}

#[tokio::test]
async fn update_service_scale_in_skips_protected_tasks() {
    if !require_docker_or_skip("update_service_scale_in_skips_protected_tasks") {
        return;
    }
    let server = TestServer::start().await;
    let client = server.ecs_client().await;
    bootstrap(&client, "prot-cluster", "prot-td", vec![]).await;

    client
        .create_service()
        .cluster("prot-cluster")
        .service_name("web")
        .task_definition("prot-td")
        .desired_count(2)
        .send()
        .await
        .expect("create_service");

    let listed = client
        .list_tasks()
        .cluster("prot-cluster")
        .send()
        .await
        .expect("list_tasks");
    let task_arns: Vec<String> = listed.task_arns().to_vec();
    assert_eq!(task_arns.len(), 2, "service should have spawned 2 tasks");
    let protected_arn = &task_arns[1];

    client
        .update_task_protection()
        .cluster("prot-cluster")
        .tasks(protected_arn)
        .protection_enabled(true)
        .send()
        .await
        .expect("update_task_protection");

    client
        .update_service()
        .cluster("prot-cluster")
        .service("web")
        .desired_count(1)
        .send()
        .await
        .expect("update_service down");

    // The protected task must still exist (RUNNING/PENDING — not STOPPED).
    let described = client
        .describe_tasks()
        .cluster("prot-cluster")
        .tasks(protected_arn)
        .send()
        .await
        .expect("describe_tasks");
    let task = described.tasks().first().expect("protected task present");
    assert_ne!(
        task.last_status(),
        Some("STOPPED"),
        "protected task must survive scale-in; got last_status={:?}",
        task.last_status()
    );
    assert_ne!(
        task.desired_status(),
        Some("STOPPED"),
        "protected task desired_status must not flip to STOPPED on scale-in; got {:?}",
        task.desired_status()
    );
}

// ── Daemon task spawn (O10) ──────────────────────────────────────────────

async fn daemon_tasks_for_cluster(server: &TestServer, cluster: &str) -> Vec<serde_json::Value> {
    let url = format!(
        "{}/_fakecloud/ecs/tasks?cluster={}",
        server.endpoint(),
        cluster
    );
    let resp = reqwest::get(&url).await.unwrap();
    let text = resp.text().await.unwrap();
    let body: serde_json::Value = serde_json::from_str(&text).unwrap();
    body.get("tasks")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
}

#[tokio::test]
async fn daemon_spawns_one_task_per_capacity_provider() {
    let server = TestServer::start().await;
    let client = server.ecs_client().await;

    client
        .create_cluster()
        .cluster_name("default")
        .send()
        .await
        .unwrap();

    let td_arn = client
        .register_daemon_task_definition()
        .family("d-td")
        .container_definitions(
            aws_sdk_ecs::types::DaemonContainerDefinition::builder()
                .name("agent")
                .image("nginx:latest")
                .build()
                .unwrap(),
        )
        .send()
        .await
        .unwrap()
        .daemon_task_definition_arn()
        .unwrap()
        .to_string();

    let daemon_arn = client
        .create_daemon()
        .daemon_name("d1")
        .daemon_task_definition_arn(&td_arn)
        .capacity_provider_arns("FARGATE")
        .send()
        .await
        .unwrap()
        .daemon_arn()
        .unwrap()
        .to_string();

    let url = format!("{}/_fakecloud/ecs/tasks?cluster=default", server.endpoint());
    let resp = reqwest::get(&url).await.unwrap();
    let text = resp.text().await.unwrap();
    eprintln!("TASKS AFTER CREATE: {}", text);

    let tasks = daemon_tasks_for_cluster(&server, "default").await;
    let daemon_tasks: Vec<_> = tasks
        .iter()
        .filter(|t| t.get("group").and_then(|v| v.as_str()) == Some("daemon:d1"))
        .collect();
    assert_eq!(
        daemon_tasks.len(),
        1,
        "daemon with one capacity provider should spawn exactly one task; got {daemon_tasks:?}"
    );

    // Update to two capacity providers.
    let new_td = client
        .register_daemon_task_definition()
        .family("d-td2")
        .container_definitions(
            aws_sdk_ecs::types::DaemonContainerDefinition::builder()
                .name("agent")
                .image("nginx:latest")
                .build()
                .unwrap(),
        )
        .send()
        .await
        .unwrap()
        .daemon_task_definition_arn()
        .unwrap()
        .to_string();

    client
        .update_daemon()
        .daemon_arn(&daemon_arn)
        .daemon_task_definition_arn(&new_td)
        .capacity_provider_arns("FARGATE")
        .capacity_provider_arns("FARGATE_SPOT")
        .send()
        .await
        .unwrap();

    let tasks_after = daemon_tasks_for_cluster(&server, "default").await;
    let new_daemon_tasks: Vec<_> = tasks_after
        .iter()
        .filter(|t| {
            t.get("group").and_then(|v| v.as_str()) == Some("daemon:d1")
                && t.get("taskDefinitionArn")
                    .and_then(|v| v.as_str())
                    .map(|arn| arn.contains("d-td2"))
                    .unwrap_or(false)
        })
        .collect();
    assert_eq!(
        new_daemon_tasks.len(),
        2,
        "updated daemon with two providers should spawn exactly two tasks; got {new_daemon_tasks:?}"
    );

    // Delete daemon should stop all its tasks.
    client
        .delete_daemon()
        .daemon_arn(&daemon_arn)
        .send()
        .await
        .unwrap();

    let tasks_after_del = daemon_tasks_for_cluster(&server, "default").await;
    let del_daemon_tasks: Vec<_> = tasks_after_del
        .iter()
        .filter(|t| t.get("group").and_then(|v| v.as_str()) == Some("daemon:d1"))
        .collect();
    assert!(
        del_daemon_tasks
            .iter()
            .all(|t| { t.get("desiredStatus").and_then(|v| v.as_str()) == Some("STOPPED") }),
        "all daemon tasks should be STOPPED after delete_daemon; got {del_daemon_tasks:?}"
    );
}

// ── CODE_DEPLOY blue/green (O16) ────────────────────────────────────────

#[tokio::test]
async fn codedeploy_service_creates_primary_task_set() {
    let server = TestServer::start().await;
    let client = server.ecs_client().await;
    client
        .create_cluster()
        .cluster_name("bg-cluster")
        .send()
        .await
        .unwrap();
    client
        .register_task_definition()
        .family("bg-td")
        .container_definitions(
            ContainerDefinition::builder()
                .name("app")
                .image("public.ecr.aws/docker/library/alpine:latest")
                .essential(true)
                .build(),
        )
        .send()
        .await
        .unwrap();

    let dc = aws_sdk_ecs::types::DeploymentController::builder()
        .r#type(aws_sdk_ecs::types::DeploymentControllerType::CodeDeploy)
        .build()
        .unwrap();
    client
        .create_service()
        .cluster("bg-cluster")
        .service_name("web")
        .task_definition("bg-td")
        .desired_count(2)
        .deployment_controller(dc)
        .send()
        .await
        .expect("create_service CODE_DEPLOY");

    let described = client
        .describe_services()
        .cluster("bg-cluster")
        .services("web")
        .send()
        .await
        .expect("describe_services");
    let svc = described.services().first().expect("service present");
    let sets = svc.task_sets();
    assert_eq!(
        sets.len(),
        1,
        "CODE_DEPLOY service should have one PRIMARY task set; got {sets:?}"
    );
    let ts = &sets[0];
    assert_eq!(
        ts.status(),
        Some("PRIMARY"),
        "task set should be PRIMARY; got {:?}",
        ts.status()
    );
    assert!(
        ts.task_definition().unwrap_or("").contains("bg-td"),
        "PRIMARY task set should reference bg-td; got {:?}",
        ts.task_definition()
    );
    assert_eq!(
        svc.deployments().len(),
        0,
        "CODE_DEPLOY service should have no deployments; got {:?}",
        svc.deployments()
    );
}

#[tokio::test]
async fn codedeploy_update_service_flips_primary_task_set() {
    let server = TestServer::start().await;
    let client = server.ecs_client().await;
    client
        .create_cluster()
        .cluster_name("bg-up-cluster")
        .send()
        .await
        .unwrap();
    client
        .register_task_definition()
        .family("bg-td")
        .container_definitions(
            ContainerDefinition::builder()
                .name("app")
                .image("public.ecr.aws/docker/library/alpine:latest")
                .essential(true)
                .build(),
        )
        .send()
        .await
        .unwrap();
    client
        .register_task_definition()
        .family("bg-td2")
        .container_definitions(
            ContainerDefinition::builder()
                .name("app")
                .image("public.ecr.aws/docker/library/alpine:latest")
                .essential(true)
                .build(),
        )
        .send()
        .await
        .unwrap();

    let dc = aws_sdk_ecs::types::DeploymentController::builder()
        .r#type(aws_sdk_ecs::types::DeploymentControllerType::CodeDeploy)
        .build()
        .unwrap();
    client
        .create_service()
        .cluster("bg-up-cluster")
        .service_name("web")
        .task_definition("bg-td")
        .desired_count(2)
        .deployment_controller(dc)
        .send()
        .await
        .expect("create_service CODE_DEPLOY");

    // Update service to new task definition.
    client
        .update_service()
        .cluster("bg-up-cluster")
        .service("web")
        .task_definition("bg-td2")
        .send()
        .await
        .expect("update_service");

    let described = client
        .describe_services()
        .cluster("bg-up-cluster")
        .services("web")
        .send()
        .await
        .expect("describe_services");
    let svc = described.services().first().expect("service present");
    let sets: Vec<_> = svc.task_sets().to_vec();
    assert_eq!(
        sets.len(),
        2,
        "after update there should be 2 task sets; got {sets:?}"
    );
    let primary = sets
        .iter()
        .find(|ts| ts.status() == Some("PRIMARY"))
        .expect("one PRIMARY task set");
    assert!(
        primary.task_definition().unwrap_or("").contains("bg-td2"),
        "PRIMARY should reference new TD bg-td2; got {:?}",
        primary.task_definition()
    );
    let old = sets
        .iter()
        .find(|ts| ts.status() == Some("ACTIVE"))
        .expect("old task set demoted to ACTIVE");
    assert!(
        old.task_definition().unwrap_or("").contains("bg-td"),
        "ACTIVE should reference old TD bg-td; got {:?}",
        old.task_definition()
    );
    assert_eq!(
        old.computed_desired_count(),
        0,
        "old task set should have computedDesiredCount=0; got {:?}",
        old.computed_desired_count()
    );

    // Old tasks on bg-td should be desired STOPPED.
    let tasks = client
        .list_tasks()
        .cluster("bg-up-cluster")
        .send()
        .await
        .expect("list_tasks");
    //
    for arn in tasks.task_arns() {
        let d = client
            .describe_tasks()
            .cluster("bg-up-cluster")
            .tasks(arn)
            .send()
            .await
            .expect("describe_tasks");
        let t = d.tasks().first().expect("task");
        let td = t.task_definition_arn().unwrap_or("");
        if td.contains("bg-td") && !td.contains("bg-td2") {
            assert_eq!(
                t.desired_status(),
                Some("STOPPED"),
                "old-TD task should be desired STOPPED; got {:?}",
                t.desired_status()
            );
        }
    }
}
