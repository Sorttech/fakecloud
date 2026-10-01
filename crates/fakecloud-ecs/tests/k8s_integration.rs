//! Opt-in Kubernetes integration tests for the ECS k8s backend.
//!
//! Needs a real cluster (a local `kind` cluster works) with `busybox:1.36`
//! loaded (and `public.ecr.aws/docker/library/alpine:3.20` pullable, plus
//! egress to the Alpine package mirror, for the credentials initContainer),
//! plus a valid kubeconfig. Gated behind the `k8s-integration` feature.
//!
//! Per `feedback_tests_never_silently_skip`: with the feature on, a
//! missing `FAKECLOUD_K8S_TEST=1` / unreachable cluster **panics** rather
//! than silently passing.
//!
//! These validate the k8s primitives the ECS backend's task lifecycle is
//! built on — multi-container Pods with an initContainer (the
//! `dependsOn` COMPLETE/SUCCESS mapping), per-container log capture
//! (`pod_logs`, used by `k8s_finalize`), terminal-status reading, and
//! label reaping — against a real cluster.
//!
//! Run with:
//! ```sh
//! kind create cluster --name fakecloud-test
//! docker pull busybox:1.36 && kind load docker-image busybox:1.36 --name fakecloud-test
//! docker pull public.ecr.aws/docker/library/alpine:3.20 \
//!     && kind load docker-image public.ecr.aws/docker/library/alpine:3.20 --name fakecloud-test
//! FAKECLOUD_K8S_TEST=1 cargo test -p fakecloud-ecs \
//!     --features k8s-integration --test k8s_integration -- --test-threads=1
//! ```

#![cfg(feature = "k8s-integration")]

use std::collections::BTreeMap;
use std::time::Duration;

use k8s_openapi::api::core::v1::{Container, Pod, PodSpec};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;

use fakecloud_ecs::runtime::EcsRuntime;
use fakecloud_k8s::{labels, K8sClient};

const TEST_NS: &str = "fakecloud-ecs-test";

fn require_test_env() {
    if std::env::var("FAKECLOUD_K8S_TEST").is_err() {
        panic!(
            "FAKECLOUD_K8S_TEST not set — refusing to silently skip k8s integration tests.\n\
             kind create cluster --name fakecloud-test\n  \
             kind load docker-image busybox:1.36 --name fakecloud-test\n  \
             FAKECLOUD_K8S_TEST=1 cargo test -p fakecloud-ecs \\\n      \
                 --features k8s-integration --test k8s_integration -- --test-threads=1"
        );
    }
}

async fn client() -> K8sClient {
    K8sClient::connect(TEST_NS.to_string())
        .await
        .expect("connect to cluster — set KUBECONFIG or run inside a cluster")
}

async fn ensure_namespace() {
    use k8s_openapi::api::core::v1::Namespace;
    use kube::api::{Api, PostParams};
    let c = K8sClient::connect("default".to_string()).await.unwrap();
    let api: Api<Namespace> = Api::all(c.client().clone());
    let ns = Namespace {
        metadata: ObjectMeta {
            name: Some(TEST_NS.into()),
            ..Default::default()
        },
        ..Default::default()
    };
    match api.create(&PostParams::default(), &ns).await {
        Ok(_) => {}
        Err(kube::Error::Api(e)) if e.code == 409 => {}
        Err(e) => panic!("create test namespace: {e}"),
    }
}

fn busybox(name: &str, args: &str) -> Container {
    Container {
        name: name.into(),
        image: Some("busybox:1.36".into()),
        command: Some(vec!["sh".into(), "-c".into(), args.into()]),
        ..Default::default()
    }
}

/// A task-shaped Pod: one initContainer (the COMPLETE/SUCCESS dependency
/// mapping) + one app container, both echoing a marker then exiting.
fn task_pod(name: &str) -> Pod {
    let mut l = BTreeMap::new();
    l.insert(
        labels::MANAGED_BY.to_string(),
        labels::MANAGED_BY_VALUE.to_string(),
    );
    l.insert(labels::SERVICE.to_string(), "ecs".to_string());
    l.insert(labels::INSTANCE.to_string(), labels::instance_id());
    Pod {
        metadata: ObjectMeta {
            name: Some(name.into()),
            namespace: Some(TEST_NS.into()),
            labels: Some(l),
            ..Default::default()
        },
        spec: Some(PodSpec {
            restart_policy: Some("Never".into()),
            init_containers: Some(vec![busybox("migrate", "echo INIT_RAN; exit 0")]),
            containers: vec![busybox("app", "echo APP_RAN; exit 0")],
            ..Default::default()
        }),
        ..Default::default()
    }
}

#[tokio::test]
async fn precondition_env_must_be_set() {
    require_test_env();
}

#[tokio::test]
async fn new_k8s_constructs_and_reports_kubernetes() {
    require_test_env();
    std::env::set_var(
        "FAKECLOUD_K8S_SELF_URL",
        "http://fakecloud.fakecloud-ecs-test.svc.cluster.local:4566",
    );
    std::env::set_var("FAKECLOUD_K8S_NAMESPACE", TEST_NS);
    let rt = EcsRuntime::new_k8s(4566).await.expect("new_k8s");
    assert_eq!(rt.cli_name(), "kubernetes");
    rt.reap_stale().await;
}

#[tokio::test]
async fn task_pod_runs_init_then_app_and_logs_are_captured() {
    require_test_env();
    ensure_namespace().await;
    let c = client().await;
    let name = "fakecloud-ecs-it-task";
    c.delete_pod(name).await;
    c.create_pod(&task_pod(name))
        .await
        .expect("create task pod");

    // Wait for the Pod to reach a terminal phase (both containers exit 0).
    let mut succeeded = false;
    for _ in 0..120 {
        if let Ok(pod) = c.pods().get(name).await {
            if pod.status.as_ref().and_then(|s| s.phase.as_deref()) == Some("Succeeded") {
                succeeded = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(succeeded, "task pod did not reach Succeeded");

    // Per-container logs (the path k8s_finalize uses to populate
    // captured_logs) — init container ran before the app container.
    let init_logs = c.pod_logs(name, Some("migrate")).await.expect("init logs");
    assert!(init_logs.contains("INIT_RAN"), "init logs: {init_logs:?}");
    let app_logs = c.pod_logs(name, Some("app")).await.expect("app logs");
    assert!(app_logs.contains("APP_RAN"), "app logs: {app_logs:?}");

    c.delete_pod(name).await;
}

#[tokio::test]
async fn reap_stale_deletes_foreign_instance_pods() {
    require_test_env();
    ensure_namespace().await;
    let c = client().await;
    let name = "fakecloud-ecs-it-foreign";
    c.delete_pod(name).await;
    let mut pod = task_pod(name);
    // Long-lived so it doesn't exit before we reap it, but exits promptly
    // on SIGTERM so deletion completes within the poll window (a bare
    // `sleep` ignores TERM and waits out the 30s grace period).
    pod.spec.as_mut().unwrap().containers =
        vec![busybox("app", "trap 'exit 0' TERM; sleep 300 & wait")];
    pod.spec.as_mut().unwrap().init_containers = None;
    pod.metadata
        .labels
        .as_mut()
        .unwrap()
        .insert(labels::INSTANCE.to_string(), "fakecloud-99999".to_string());
    c.create_pod(&pod).await.expect("create foreign pod");

    let reaped = c.reap_stale("ecs").await;
    assert!(reaped >= 1, "expected to reap the foreign pod");

    for _ in 0..60 {
        if c.pods().get_opt(name).await.unwrap().is_none() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    panic!("foreign pod still present after reap");
}

/// The task-role credentials initContainer really routes the ECS agent's
/// `169.254.170.2:80` to its target inside the Pod: a later container
/// fetching `http://169.254.170.2/v2/credentials/<id>` reaches a stand-in
/// server behind a Service, on another port, while port 80 in the Pod stays
/// free for the app.
#[tokio::test]
async fn creds_init_container_routes_the_agent_address_to_its_target() {
    use k8s_openapi::api::core::v1::{Service, ServicePort, ServiceSpec};
    use kube::api::{Api, DeleteParams, PostParams};

    require_test_env();
    ensure_namespace().await;
    let c = client().await;
    let services: Api<Service> = Api::namespaced(c.client().clone(), TEST_NS);
    let server = "fakecloud-ecs-it-creds-srv";
    let task = "fakecloud-ecs-it-creds-task";
    // Pods and a Service left by an interrupted run must be gone before we
    // recreate them, or the create is a 409 while they terminate.
    for pod in [server, task] {
        c.delete_pod(pod).await;
        let mut gone = false;
        for _ in 0..120 {
            if c.pods().get_opt(pod).await.unwrap().is_none() {
                gone = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        assert!(gone, "stale pod {pod} was not deleted");
    }
    // A Service left by an interrupted run must be gone before we recreate
    // it, or the create is a 409 while it terminates.
    let _ = services.delete(server, &DeleteParams::default()).await;
    let mut gone = false;
    for _ in 0..60 {
        if services.get_opt(server).await.unwrap().is_none() {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(gone, "stale service {server} was not deleted");

    // Stand-in for fakecloud: serves the agent path on 8080.
    let mut srv = task_pod(server);
    let spec = srv.spec.as_mut().unwrap();
    spec.init_containers = None;
    spec.containers = vec![busybox(
        "srv",
        "mkdir -p /www/v2/credentials && echo CREDS_OK > /www/v2/credentials/t1 \
         && trap 'exit 0' TERM; httpd -f -p 8080 -h /www & wait",
    )];
    srv.metadata
        .labels
        .as_mut()
        .unwrap()
        .insert("app".into(), server.into());
    c.create_pod(&srv).await.expect("create server pod");
    services
        .create(
            &PostParams::default(),
            &Service {
                metadata: ObjectMeta {
                    name: Some(server.into()),
                    namespace: Some(TEST_NS.into()),
                    ..Default::default()
                },
                spec: Some(ServiceSpec {
                    selector: Some(BTreeMap::from([("app".to_string(), server.to_string())])),
                    ports: Some(vec![ServicePort {
                        port: 8080,
                        ..Default::default()
                    }]),
                    ..Default::default()
                }),
                ..Default::default()
            },
        )
        .await
        .expect("create server service");

    let mut pod = task_pod(task);
    let spec = pod.spec.as_mut().unwrap();
    spec.init_containers = Some(vec![fakecloud_ecs::runtime::k8s_creds_init_container(
        "public.ecr.aws/docker/library/alpine:3.20",
        &format!("{server}.{TEST_NS}.svc.cluster.local"),
        8080,
    )]);
    spec.containers = vec![busybox(
        "app",
        "nc -l -p 80 -e true & nc_pid=$!; sleep 1; \
         kill -0 \"$nc_pid\" || { echo PORT80_BIND_FAILED; exit 1; }; \
         for i in $(seq 60); do \
           wget -qO- http://169.254.170.2/v2/credentials/t1 && exit 0; sleep 1; \
         done; exit 1",
    )];
    c.create_pod(&pod).await.expect("create task pod");

    let mut phase = String::new();
    for _ in 0..240 {
        if let Ok(p) = c.pods().get(task).await {
            phase = p
                .status
                .as_ref()
                .and_then(|s| s.phase.clone())
                .unwrap_or_default();
            if phase == "Succeeded" || phase == "Failed" {
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let init_logs = c
        .pod_logs(task, Some("fakecloud-ecs-creds"))
        .await
        .unwrap_or_default();
    let app_logs = c.pod_logs(task, Some("app")).await.unwrap_or_default();
    assert_eq!(
        phase, "Succeeded",
        "init logs: {init_logs:?}, app logs: {app_logs:?}"
    );
    assert!(
        init_logs.contains("FAKECLOUD_ECS_CREDS_READY"),
        "init logs: {init_logs:?}"
    );
    assert!(app_logs.contains("CREDS_OK"), "app logs: {app_logs:?}");

    c.delete_pod(task).await;
    c.delete_pod(server).await;
    let _ = services.delete(server, &DeleteParams::default()).await;
}

/// A task stopped while its credentials initContainer is failing is not
/// relaunched (there is nothing to run) and goes straight to STOPPED with
/// the user's stop reason, never RUNNING.
#[tokio::test]
async fn task_stopped_during_failed_creds_init_never_runs() {
    use fakecloud_core::multi_account::MultiAccountState;
    use fakecloud_ecs::{EcsState, SharedEcsState, Task, TaskDefinition};

    require_test_env();
    ensure_namespace().await;
    std::env::set_var(
        "FAKECLOUD_K8S_SELF_URL",
        "http://fakecloud.fakecloud-ecs-test.svc.cluster.local:4566",
    );
    std::env::set_var("FAKECLOUD_K8S_NAMESPACE", TEST_NS);
    // A helper image that can never be pulled: the initContainer fails, which
    // would normally relaunch the task without it. Unset on drop, so a failed
    // assertion can't leak it into later tests in this binary.
    struct HelperImageOverride;
    impl Drop for HelperImageOverride {
        fn drop(&mut self) {
            std::env::remove_var("FAKECLOUD_ECS_CREDS_HELPER_IMAGE");
        }
    }
    std::env::set_var(
        "FAKECLOUD_ECS_CREDS_HELPER_IMAGE",
        "registry.invalid/fakecloud/no-such-helper:1",
    );
    let _override = HelperImageOverride;
    let rt = EcsRuntime::new_k8s(4566).await.expect("new_k8s");

    let account = "123456789012";
    let task_id = "0123456789abcdef0123456789abcd01";
    let state: SharedEcsState =
        std::sync::Arc::new(parking_lot::RwLock::new(
            MultiAccountState::<EcsState>::new(account, "us-east-1", "http://localhost:4566"),
        ));
    let td: TaskDefinition = serde_json::from_value(serde_json::json!({
        "family": "it-creds",
        "revision": 1,
        "task_definition_arn": format!("arn:aws:ecs:us-east-1:{account}:task-definition/it-creds:1"),
        "container_definitions": [{
            "name": "app",
            "image": "busybox:1.36",
            "essential": true,
            "command": ["sh", "-c", "sleep 300"],
        }],
        "status": "ACTIVE",
        "task_role_arn": format!("arn:aws:iam::{account}:role/app"),
        "network_mode": "awsvpc",
        "registered_at": chrono::Utc::now(),
    }))
    .expect("task definition");
    let task: Task = serde_json::from_value(serde_json::json!({
        "task_arn": format!("arn:aws:ecs:us-east-1:{account}:task/c/{task_id}"),
        "task_id": task_id,
        "cluster_arn": format!("arn:aws:ecs:us-east-1:{account}:cluster/c"),
        "cluster_name": "c",
        "task_definition_arn": format!("arn:aws:ecs:us-east-1:{account}:task-definition/it-creds:1"),
        "family": "it-creds",
        "revision": 1,
        "last_status": "PENDING",
        // StopTask already landed.
        "desired_status": "STOPPED",
        "stop_code": "UserInitiated",
        "stopped_reason": "stopped by test",
        "launch_type": "FARGATE",
        "containers": [{
            "container_arn": format!("arn:aws:ecs:us-east-1:{account}:container/c/{task_id}/app"),
            "name": "app",
            "image": "busybox:1.36",
            "task_arn": format!("arn:aws:ecs:us-east-1:{account}:task/c/{task_id}"),
            "last_status": "PENDING",
            "essential": true,
        }],
        "overrides": {},
        "connectivity": "CONNECTING",
        "created_at": chrono::Utc::now(),
        "task_role_arn": format!("arn:aws:iam::{account}:role/app"),
        "tags": [],
    }))
    .expect("task");
    {
        let mut accounts = state.write();
        let s = accounts.get_or_create(account);
        s.task_definitions
            .entry("it-creds".into())
            .or_default()
            .insert(1, td);
        s.tasks.insert(task_id.into(), task);
    }

    rt.run_task_inner(&state, task_id, account)
        .await
        .expect("run_task_inner");

    {
        let accounts = state.read();
        let task = accounts.get(account).unwrap().tasks.get(task_id).unwrap();
        assert_eq!(task.last_status, "STOPPED");
        assert!(task.started_at.is_none(), "task was marked RUNNING");
        assert_eq!(task.stop_code.as_deref(), Some("UserInitiated"));
        assert_eq!(task.stopped_reason.as_deref(), Some("stopped by test"));
    }
    // No relaunched (full-URI) Pod: the only Pod the task had is on its way
    // out.
    let pods = client().await.pods();
    let lp = kube::api::ListParams::default().labels(&format!("fakecloud-ecs-task={task_id}"));
    for pod in pods.list(&lp).await.unwrap().items {
        assert!(
            pod.metadata.deletion_timestamp.is_some(),
            "live pod left behind: {:?}",
            pod.metadata.name
        );
        assert!(
            !pod.metadata
                .name
                .as_deref()
                .unwrap_or_default()
                .contains("full-uri"),
            "task was relaunched: {:?}",
            pod.metadata.name
        );
    }
}
