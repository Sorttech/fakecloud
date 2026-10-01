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
    c.delete_pod(server).await;
    c.delete_pod(task).await;
    let _ = services.delete(server, &DeleteParams::default()).await;

    // Stand-in for fakecloud: serves the agent path on 8080.
    let mut srv = task_pod(server);
    let spec = srv.spec.as_mut().unwrap();
    spec.init_containers = None;
    spec.containers = vec![busybox(
        "srv",
        "mkdir -p /www/v2/credentials && echo CREDS_OK > /www/v2/credentials/t1 \
         && exec httpd -f -p 8080 -h /www",
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
        "nc -l -p 80 -e true & sleep 1; \
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
