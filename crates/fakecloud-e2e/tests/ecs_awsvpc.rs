//! ECS awsvpc network mode (O7).
//!
//! Verifies that tasks with `networkMode=awsvpc` get a synthetic ENI
//! attachment and use a per-task docker network.

mod helpers;

use aws_sdk_ecs::types::{ContainerDefinition, NetworkMode};
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

#[tokio::test]
async fn ecs_awsvpc_task_gets_eni_attachment() {
    if !require_docker_or_skip("ecs_awsvpc_task_gets_eni_attachment") {
        return;
    }
    let server = TestServer::start().await;
    let ecs = server.ecs_client().await;

    ecs.create_cluster()
        .cluster_name("awsvpc-cluster")
        .send()
        .await
        .expect("create_cluster");

    ecs.register_task_definition()
        .family("awsvpc-family")
        .network_mode(NetworkMode::Awsvpc)
        .container_definitions(
            ContainerDefinition::builder()
                .name("app")
                .image("public.ecr.aws/docker/library/alpine:3.20")
                .essential(true)
                .command("sh")
                .command("-c")
                .command("sleep 1")
                .build(),
        )
        .send()
        .await
        .expect("register_task_definition");

    let run = ecs
        .run_task()
        .cluster("awsvpc-cluster")
        .task_definition("awsvpc-family")
        .send()
        .await
        .expect("run_task");
    let arn = run.tasks()[0].task_arn().unwrap().to_string();

    // Poll until STOPPED so the runtime has had time to create the ENI.
    for _ in 0..60 {
        let desc = ecs
            .describe_tasks()
            .cluster("awsvpc-cluster")
            .tasks(arn.clone())
            .send()
            .await
            .expect("describe_tasks");
        let t = &desc.tasks()[0];
        if t.last_status() == Some("STOPPED") {
            let attachments = t.attachments();
            assert!(
                attachments.iter().any(|a| a.r#type() == Some("eni")),
                "expected ENI attachment for awsvpc task; got {attachments:?}. \
                 stopCode={:?} stoppedReason={:?} containers={:?} -- a task that \
                 never started (image pull refused, registry allowance exhausted) \
                 reports no attachments, so check the reason before the ENI code",
                t.stop_code(),
                t.stopped_reason(),
                t.containers()
            );
            let eni = attachments
                .iter()
                .find(|a| a.r#type() == Some("eni"))
                .unwrap();
            assert_eq!(eni.status(), Some("ATTACHED"));
            let details: std::collections::HashMap<&str, &str> = eni
                .details()
                .iter()
                .filter_map(|d| Some((d.name()?, d.value()?)))
                .collect();
            assert!(
                details.contains_key("privateIPv4Address"),
                "expected privateIPv4Address in ENI details: {details:?}"
            );
            assert!(
                details.contains_key("macAddress"),
                "expected macAddress in ENI details: {details:?}"
            );
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    panic!("task never reached STOPPED");
}
