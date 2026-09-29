mod helpers;

use aws_sdk_ecs::types::ContainerDefinition;
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
    eprintln!("Skipping {test}: docker not available");
    false
}

/// Persisted RUNNING tasks must be reconciled to STOPPED on restart —
/// their docker containers are gone and reporting them as RUNNING is a
/// lie. Same restart-bug class as RDS #1338.
#[tokio::test]
async fn persistence_running_tasks_reconciled_to_stopped_after_restart() {
    if !require_docker_or_skip("persistence_running_tasks_reconciled_to_stopped_after_restart") {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let data_path = tmp.path().display().to_string();
    let extra_args = ["--storage-mode", "persistent", "--data-path", &data_path];
    let mut server = TestServer::start_full(&[], &extra_args).await;
    let client = server.ecs_client().await;

    client
        .create_cluster()
        .cluster_name("reconcile-cluster")
        .send()
        .await
        .unwrap();

    client
        .register_task_definition()
        .family("reconcile-family")
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

    let arn = client
        .run_task()
        .cluster("reconcile-cluster")
        .task_definition("reconcile-family")
        .send()
        .await
        .expect("run_task")
        .tasks()
        .first()
        .and_then(|t| t.task_arn())
        .expect("task arn")
        .to_string();

    drop(client);
    server.restart().await;
    let client = server.ecs_client().await;

    let resp = client
        .describe_tasks()
        .cluster("reconcile-cluster")
        .tasks(arn.clone())
        .send()
        .await
        .expect("describe_tasks");
    let task = &resp.tasks()[0];
    assert_eq!(
        task.last_status(),
        Some("STOPPED"),
        "persisted task must reconcile to STOPPED after restart, not phantom-RUNNING",
    );
    assert_eq!(task.desired_status(), Some("STOPPED"));
    assert!(
        task.stopped_reason()
            .is_some_and(|reason| reason.contains("restart")),
        "stoppedReason should explain the restart, got {:?}",
        task.stopped_reason(),
    );
}
