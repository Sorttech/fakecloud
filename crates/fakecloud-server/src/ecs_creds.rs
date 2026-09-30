//! The ECS task-role credentials endpoint (`GET /_fakecloud/ecs/creds/{task_id}`).
//!
//! A task whose task definition names a `taskRoleArn` gets
//! `AWS_CONTAINER_CREDENTIALS_FULL_URI` pointed here. Like the ECS agent, the
//! endpoint hands out credentials for that role's session named after the
//! task ID (`assumed-role/<role>/<task-id>`), minted and registered like an
//! `AssumeRole` session so they verify under `--verify-sigv4` and are
//! evaluated as the role under `--iam`. Refetches within the validity window
//! return the same set; near expiry a fresh one is minted.
//!
//! Once the task stops its credentials are revoked (the sweep in
//! [`run_revocation_sweep`]), and the endpoint answers a stopped, unknown, or
//! role-less task the way the agent answers an ID it holds no credentials
//! for: HTTP 400 `{"code":"InvalidIdInRequest","message":"CredentialsV2Request:
//! Credentials not found","HTTPErrorCode":400}`.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use fakecloud_ecs::SharedEcsState;
use fakecloud_iam::sts_service::container_creds::{
    ContainerCredentials, WorkloadCredentialCache, DEFAULT_CONTAINER_CREDENTIALS_DURATION,
};
use fakecloud_iam::SharedIamState;

/// Error prefix the ECS agent's v2 credentials handler puts on its messages.
const ERR_PREFIX: &str = "CredentialsV2Request: ";

/// How often stopped tasks' credentials are revoked.
const SWEEP_INTERVAL: Duration = Duration::from_secs(1);

/// Why the endpoint has no credentials for a request, as the agent reports it.
#[derive(Debug, PartialEq, Eq)]
pub enum CredentialsError {
    /// The request carried no task ID.
    NoId,
    /// No running task with that ID has a task role.
    NotFound,
}

impl IntoResponse for CredentialsError {
    fn into_response(self) -> Response {
        let (code, message) = match self {
            Self::NoId => ("NoIdInRequest", "No Credential ID in the request"),
            Self::NotFound => ("InvalidIdInRequest", "Credentials not found"),
        };
        (
            StatusCode::BAD_REQUEST,
            axum::Json(serde_json::json!({
                "code": code,
                "message": format!("{ERR_PREFIX}{message}"),
                "HTTPErrorCode": StatusCode::BAD_REQUEST.as_u16(),
            })),
        )
            .into_response()
    }
}

/// Serves task-role credentials for ECS tasks and revokes them once the task
/// stops.
pub struct EcsTaskCredentials {
    ecs: SharedEcsState,
    iam: SharedIamState,
    default_account_id: String,
    cache: WorkloadCredentialCache,
}

/// A task that may hold credentials: its account, and its role while it runs.
struct RunningTask {
    account_id: String,
    role_arn: Option<String>,
}

fn cache_key(account_id: &str, task_id: &str) -> String {
    format!("{account_id}/{task_id}")
}

impl EcsTaskCredentials {
    pub fn new(
        ecs: SharedEcsState,
        iam: SharedIamState,
        default_account_id: impl Into<String>,
    ) -> Arc<Self> {
        Arc::new(Self {
            ecs,
            iam,
            default_account_id: default_account_id.into(),
            cache: WorkloadCredentialCache::new(),
        })
    }

    /// The not-yet-stopped task `task_id`, in whichever account runs it.
    fn running_task(&self, task_id: &str) -> Option<RunningTask> {
        let accounts = self.ecs.read();
        let found = accounts.iter().find_map(|(account_id, state)| {
            let task = state.tasks.get(task_id)?;
            (task.last_status != "STOPPED").then(|| RunningTask {
                account_id: account_id.to_string(),
                role_arn: task.task_role_arn.clone(),
            })
        });
        found
    }

    /// Credentials for the task role of the running task `task_id`.
    pub fn credentials(&self, task_id: &str) -> Result<ContainerCredentials, CredentialsError> {
        if task_id.is_empty() {
            return Err(CredentialsError::NoId);
        }
        let task = self
            .running_task(task_id)
            .ok_or(CredentialsError::NotFound)?;
        let role_arn = task.role_arn.ok_or(CredentialsError::NotFound)?;
        Ok(self.cache.get_or_mint(
            &self.iam,
            &self.default_account_id,
            &cache_key(&task.account_id, task_id),
            &role_arn,
            task_id,
            DEFAULT_CONTAINER_CREDENTIALS_DURATION,
        ))
    }

    /// Revoke the credentials of every task that has stopped (or is gone).
    ///
    /// The ECS read lock is held until the revocation is done, so a task
    /// started (and handed credentials) while the sweep runs is never judged
    /// against a snapshot that predates it. Lock order: ECS state, then the
    /// cache, then IAM; `credentials` releases the ECS lock before touching
    /// the cache, so the two never wait on each other in reverse.
    pub fn revoke_stopped(&self) {
        let accounts = self.ecs.read();
        let running: HashSet<String> = accounts
            .iter()
            .flat_map(|(account_id, state)| {
                state
                    .tasks
                    .iter()
                    .filter(|(_, t)| t.last_status != "STOPPED")
                    .map(move |(task_id, _)| cache_key(account_id, task_id))
            })
            .collect();
        self.cache
            .revoke_unless(&self.iam, |key| running.contains(key));
        drop(accounts);
    }

    /// The endpoint response for `task_id`.
    pub fn respond(&self, task_id: &str) -> Response {
        match self.credentials(task_id) {
            Ok(creds) => (StatusCode::OK, axum::Json(creds.to_container_json())).into_response(),
            Err(e) => e.into_response(),
        }
    }
}

/// Revoke stopped tasks' credentials for as long as the server runs.
pub async fn run_revocation_sweep(creds: Arc<EcsTaskCredentials>) {
    let mut interval = tokio::time::interval(SWEEP_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        creds.revoke_stopped();
    }
}

/// Test fixtures shared with the link-local listener's tests.
#[cfg(test)]
pub(crate) mod test_support {
    use fakecloud_ecs::SharedEcsState;

    /// Insert a RUNNING task `task_id` into `account` with the given role.
    pub(crate) fn add_task(ecs: &SharedEcsState, account: &str, task_id: &str, role: Option<&str>) {
        let task: fakecloud_ecs::Task = serde_json::from_value(serde_json::json!({
            "task_arn": format!("arn:aws:ecs:us-east-1:{account}:task/c/{task_id}"),
            "task_id": task_id,
            "cluster_arn": format!("arn:aws:ecs:us-east-1:{account}:cluster/c"),
            "cluster_name": "c",
            "task_definition_arn": format!("arn:aws:ecs:us-east-1:{account}:task-definition/f:1"),
            "family": "f",
            "revision": 1,
            "last_status": "RUNNING",
            "desired_status": "RUNNING",
            "launch_type": "FARGATE",
            "containers": [],
            "overrides": {},
            "connectivity": "CONNECTED",
            "created_at": chrono::Utc::now(),
            "task_role_arn": role,
            "tags": [],
        }))
        .expect("task deserializes");
        ecs.write()
            .get_or_create(account)
            .tasks
            .insert(task_id.to_string(), task);
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::add_task;
    use super::*;
    use fakecloud_core::auth::CredentialResolver;
    use fakecloud_core::multi_account::MultiAccountState;
    use fakecloud_iam::credential_resolver::IamCredentialResolver;
    use parking_lot::RwLock;

    const ACCOUNT: &str = "123456789012";
    const ROLE: &str = "arn:aws:iam::123456789012:role/app-task-role";

    fn setup() -> (SharedEcsState, SharedIamState, Arc<EcsTaskCredentials>) {
        let ecs: SharedEcsState = Arc::new(RwLock::new(MultiAccountState::new(
            ACCOUNT,
            "us-east-1",
            "http://localhost:4566",
        )));
        let iam: SharedIamState = Arc::new(RwLock::new(MultiAccountState::new(
            ACCOUNT,
            "us-east-1",
            "http://localhost:4566",
        )));
        let creds = EcsTaskCredentials::new(ecs.clone(), iam.clone(), ACCOUNT);
        (ecs, iam, creds)
    }

    fn stop_task(ecs: &SharedEcsState, account: &str, task_id: &str) {
        ecs.write()
            .get_or_create(account)
            .tasks
            .get_mut(task_id)
            .unwrap()
            .last_status = "STOPPED".into();
    }

    fn resolves(iam: &SharedIamState, creds: &ContainerCredentials) -> bool {
        IamCredentialResolver::new(iam.clone())
            .resolve(&creds.access_key_id)
            .is_some()
    }

    #[test]
    fn running_task_gets_its_role_session_named_after_the_task() {
        let (ecs, iam, endpoint) = setup();
        add_task(&ecs, ACCOUNT, "abc123", Some(ROLE));
        let creds = endpoint.credentials("abc123").expect("credentials");
        assert_eq!(creds.role_arn, ROLE);
        assert_eq!(
            creds.assumed_role_arn,
            "arn:aws:sts::123456789012:assumed-role/app-task-role/abc123"
        );
        assert!(creds.access_key_id.starts_with("FSIA"), "{creds:?}");
        let resolved = IamCredentialResolver::new(iam.clone())
            .resolve(&creds.access_key_id)
            .expect("registered");
        assert_eq!(resolved.secret_access_key, creds.secret_access_key);
        assert_eq!(resolved.principal.arn, creds.assumed_role_arn);

        // Refetches within the validity window reuse the same set.
        let again = endpoint.credentials("abc123").unwrap();
        assert_eq!(again.access_key_id, creds.access_key_id);
        let json = again.to_container_json();
        for field in [
            "AccessKeyId",
            "SecretAccessKey",
            "Token",
            "Expiration",
            "RoleArn",
        ] {
            assert!(json.get(field).is_some(), "missing {field}: {json}");
        }
    }

    #[test]
    fn stopped_task_is_revoked_and_refused() {
        let (ecs, iam, endpoint) = setup();
        add_task(&ecs, ACCOUNT, "t1", Some(ROLE));
        add_task(&ecs, ACCOUNT, "t2", Some(ROLE));
        let c1 = endpoint.credentials("t1").unwrap();
        let c2 = endpoint.credentials("t2").unwrap();
        assert_ne!(c1.access_key_id, c2.access_key_id);

        stop_task(&ecs, ACCOUNT, "t1");
        endpoint.revoke_stopped();
        assert!(
            !resolves(&iam, &c1),
            "stopped task's creds still registered"
        );
        assert!(resolves(&iam, &c2), "running task's creds were revoked");
        assert_eq!(
            endpoint.credentials("t1").unwrap_err(),
            CredentialsError::NotFound
        );

        // A task removed from state (e.g. an ECS reset) is revoked too.
        ecs.write().get_or_create(ACCOUNT).tasks.clear();
        endpoint.revoke_stopped();
        assert!(!resolves(&iam, &c2));
    }

    #[test]
    fn roleless_unknown_and_empty_ids_are_refused_like_the_agent() {
        let (ecs, _iam, endpoint) = setup();
        add_task(&ecs, ACCOUNT, "norole", None);
        assert_eq!(
            endpoint.credentials("norole").unwrap_err(),
            CredentialsError::NotFound
        );
        assert_eq!(
            endpoint.credentials("missing").unwrap_err(),
            CredentialsError::NotFound
        );
        assert_eq!(
            endpoint.credentials("").unwrap_err(),
            CredentialsError::NoId
        );
    }

    #[tokio::test]
    async fn error_body_matches_the_agent() {
        let (_ecs, _iam, endpoint) = setup();
        let resp = endpoint.respond("missing");
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "code": "InvalidIdInRequest",
                "message": "CredentialsV2Request: Credentials not found",
                "HTTPErrorCode": 400,
            })
        );
    }

    #[test]
    fn iam_reset_under_the_cache_mints_fresh_registered_creds() {
        let (ecs, iam, endpoint) = setup();
        add_task(&ecs, ACCOUNT, "t", Some(ROLE));
        let before = endpoint.credentials("t").unwrap();
        iam.write().reset();
        let after = endpoint.credentials("t").unwrap();
        assert_ne!(before.access_key_id, after.access_key_id);
        assert!(resolves(&iam, &after));
    }

    #[test]
    fn task_in_another_account_mints_there() {
        let (ecs, iam, endpoint) = setup();
        let role = "arn:aws:iam::222222222222:role/other";
        add_task(&ecs, "222222222222", "x", Some(role));
        let creds = endpoint.credentials("x").unwrap();
        assert_eq!(
            creds.assumed_role_arn,
            "arn:aws:sts::222222222222:assumed-role/other/x"
        );
        assert!(resolves(&iam, &creds));
    }
}
