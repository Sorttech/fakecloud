//! AWS Batch restJson1 service dispatch + core control plane.

use std::sync::Arc;

use async_trait::async_trait;
use http::{Method, StatusCode};
use serde_json::{json, Map, Value};
use tokio::sync::Mutex as AsyncMutex;
use uuid::Uuid;

use fakecloud_aws::arn::Arn;
use fakecloud_core::service::{AwsRequest, AwsResponse, AwsService, AwsServiceError};
use fakecloud_persistence::{SnapshotHook, SnapshotStore};

use crate::state::{BatchSnapshot, BatchState, SharedBatchState, BATCH_SNAPSHOT_SCHEMA_VERSION};

const SUPPORTED_ACTIONS: &[&str] = &[
    "CancelJob",
    "CreateComputeEnvironment",
    "CreateConsumableResource",
    "CreateJobQueue",
    "CreateQuotaShare",
    "CreateSchedulingPolicy",
    "CreateServiceEnvironment",
    "DeleteComputeEnvironment",
    "DeleteConsumableResource",
    "DeleteJobQueue",
    "DeleteQuotaShare",
    "DeleteSchedulingPolicy",
    "DeleteServiceEnvironment",
    "DeregisterJobDefinition",
    "DescribeComputeEnvironments",
    "DescribeConsumableResource",
    "DescribeJobDefinitions",
    "DescribeJobQueues",
    "DescribeJobs",
    "DescribeQuotaShare",
    "DescribeSchedulingPolicies",
    "DescribeServiceEnvironments",
    "DescribeServiceJob",
    "GetJobQueueSnapshot",
    "ListConsumableResources",
    "ListJobs",
    "ListJobsByConsumableResource",
    "ListQuotaShares",
    "ListSchedulingPolicies",
    "ListServiceJobs",
    "ListTagsForResource",
    "RegisterJobDefinition",
    "SubmitJob",
    "SubmitServiceJob",
    "TagResource",
    "TerminateJob",
    "TerminateServiceJob",
    "UntagResource",
    "UpdateComputeEnvironment",
    "UpdateConsumableResource",
    "UpdateJobQueue",
    "UpdateQuotaShare",
    "UpdateSchedulingPolicy",
    "UpdateServiceEnvironment",
    "UpdateServiceJob",
];

/// Mutating actions trigger a snapshot write after success.
const MUTATING_ACTIONS: &[&str] = &[
    "CreateComputeEnvironment",
    "UpdateComputeEnvironment",
    "DeleteComputeEnvironment",
    "CreateJobQueue",
    "UpdateJobQueue",
    "DeleteJobQueue",
    "RegisterJobDefinition",
    "DeregisterJobDefinition",
    "CreateSchedulingPolicy",
    "UpdateSchedulingPolicy",
    "DeleteSchedulingPolicy",
    "SubmitJob",
    "CancelJob",
    "TerminateJob",
    "TagResource",
    "UntagResource",
    "CreateConsumableResource",
    "UpdateConsumableResource",
    "DeleteConsumableResource",
    "CreateServiceEnvironment",
    "UpdateServiceEnvironment",
    "DeleteServiceEnvironment",
    "CreateQuotaShare",
    "UpdateQuotaShare",
    "DeleteQuotaShare",
    "SubmitServiceJob",
    "TerminateServiceJob",
    "UpdateServiceJob",
];

pub struct BatchService {
    pub(crate) state: SharedBatchState,
    snapshot_store: Option<Arc<dyn SnapshotStore>>,
    snapshot_lock: Arc<AsyncMutex<()>>,
    /// ECS backend so a submitted job runs as a REAL container (the wedge:
    /// every rival fakes Batch compute). `None` parks jobs at SUBMITTED
    /// honestly (unit tests / no container runtime), never auto-succeeds.
    /// This is the server's fully wired ECS service (snapshot store, runtime,
    /// role-trust validator, IAM mode), so the cluster, task definitions and
    /// tasks a job creates persist and are validated like any ECS call.
    ecs: Option<Arc<fakecloud_ecs::EcsService>>,
}

impl BatchService {
    pub fn new(state: SharedBatchState) -> Self {
        Self {
            state,
            snapshot_store: None,
            snapshot_lock: Arc::new(AsyncMutex::new(())),
            ecs: None,
        }
    }

    pub fn with_snapshot_store(mut self, store: Arc<dyn SnapshotStore>) -> Self {
        self.snapshot_store = Some(store);
        self
    }

    /// Attach the ECS backend so SubmitJob launches a real container-backed
    /// task and drives the job status off its real exit code. Pass the same
    /// wired service the registry dispatches to, so the job's ECS resources
    /// go through its persistence and task-role checks.
    pub fn with_ecs(mut self, ecs: Arc<fakecloud_ecs::EcsService>) -> Self {
        self.ecs = Some(ecs);
        self
    }

    async fn save_snapshot(&self) {
        let Some(store) = self.snapshot_store.clone() else {
            return;
        };
        let _guard = self.snapshot_lock.lock().await;
        let bytes = {
            let snap = BatchSnapshot {
                schema_version: BATCH_SNAPSHOT_SCHEMA_VERSION,
                accounts: Some(self.state.read().clone()),
            };
            serde_json::to_vec(&snap).unwrap_or_default()
        };
        let _ = tokio::task::spawn_blocking(move || store.save(&bytes)).await;
    }

    /// CloudFormation write-through hook (used once the CFN provisioner lands).
    pub fn snapshot_hook(&self) -> Option<SnapshotHook> {
        let store = self.snapshot_store.clone()?;
        let state = self.state.clone();
        let lock = self.snapshot_lock.clone();
        Some(Arc::new(move || {
            let store = store.clone();
            let state = state.clone();
            let lock = lock.clone();
            Box::pin(async move {
                let _guard = lock.lock().await;
                let bytes = {
                    let snap = BatchSnapshot {
                        schema_version: BATCH_SNAPSHOT_SCHEMA_VERSION,
                        accounts: Some(state.read().clone()),
                    };
                    serde_json::to_vec(&snap).unwrap_or_default()
                };
                let _ = tokio::task::spawn_blocking(move || store.save(&bytes)).await;
            })
        }))
    }

    /// Reconcile jobs restored from a snapshot. After a restart the background
    /// drivers that advance a job (status-sync / dependency-waiter) are gone and
    /// the backing ECS task was already STOPPED by the ECS reconcile, so an
    /// in-flight job can never reach a terminal state on its own — it would hang
    /// at RUNNING/PENDING forever. Fail every non-terminal job with a clear
    /// reason instead of leaving a zombie.
    pub async fn reconcile_persisted_jobs(&self) {
        const NON_TERMINAL: &[&str] = &["SUBMITTED", "PENDING", "RUNNABLE", "STARTING", "RUNNING"];
        let now = chrono::Utc::now().timestamp_millis();
        let mut changed = false;
        {
            let mut accounts = self.state.write();
            for acct in accounts.accounts.values_mut() {
                for job in acct.jobs.values_mut() {
                    let Some(o) = job.as_object_mut() else {
                        continue;
                    };
                    let st = o.get("status").and_then(Value::as_str).unwrap_or("");
                    if NON_TERMINAL.contains(&st) {
                        o.insert("status".into(), json!("FAILED"));
                        o.insert(
                            "statusReason".into(),
                            json!("Job interrupted by a fakecloud restart"),
                        );
                        o.entry("stoppedAt".to_string())
                            .or_insert_with(|| json!(now));
                        changed = true;
                    }
                }
            }
        }
        if changed {
            self.save_snapshot().await;
        }
    }

    /// Map the restJson1 request (POST /v1/<op>, plus the /v1/tags/{arn}
    /// family) to its operation name.
    fn resolve_action(req: &AwsRequest) -> Option<&'static str> {
        let segs = &req.path_segments;
        if segs.first().map(|s| s.as_str()) != Some("v1") {
            return None;
        }
        // Tag family: /v1/tags/{resourceArn}
        if segs.get(1).map(|s| s.as_str()) == Some("tags") {
            return match req.method {
                Method::GET => Some("ListTagsForResource"),
                Method::POST => Some("TagResource"),
                Method::DELETE => Some("UntagResource"),
                _ => None,
            };
        }
        let op = segs.get(1)?.as_str();
        Some(match op {
            "canceljob" => "CancelJob",
            "createcomputeenvironment" => "CreateComputeEnvironment",
            "createconsumableresource" => "CreateConsumableResource",
            "createjobqueue" => "CreateJobQueue",
            "createquotashare" => "CreateQuotaShare",
            "createschedulingpolicy" => "CreateSchedulingPolicy",
            "createserviceenvironment" => "CreateServiceEnvironment",
            "deletecomputeenvironment" => "DeleteComputeEnvironment",
            "deleteconsumableresource" => "DeleteConsumableResource",
            "deletejobqueue" => "DeleteJobQueue",
            "deletequotashare" => "DeleteQuotaShare",
            "deleteschedulingpolicy" => "DeleteSchedulingPolicy",
            "deleteserviceenvironment" => "DeleteServiceEnvironment",
            "deregisterjobdefinition" => "DeregisterJobDefinition",
            "describecomputeenvironments" => "DescribeComputeEnvironments",
            "describeconsumableresource" => "DescribeConsumableResource",
            "describejobdefinitions" => "DescribeJobDefinitions",
            "describejobqueues" => "DescribeJobQueues",
            "describejobs" => "DescribeJobs",
            "describequotashare" => "DescribeQuotaShare",
            "describeschedulingpolicies" => "DescribeSchedulingPolicies",
            "describeserviceenvironments" => "DescribeServiceEnvironments",
            "describeservicejob" => "DescribeServiceJob",
            "getjobqueuesnapshot" => "GetJobQueueSnapshot",
            "listconsumableresources" => "ListConsumableResources",
            "listjobs" => "ListJobs",
            "listjobsbyconsumableresource" => "ListJobsByConsumableResource",
            "listquotashares" => "ListQuotaShares",
            "listschedulingpolicies" => "ListSchedulingPolicies",
            "listservicejobs" => "ListServiceJobs",
            "registerjobdefinition" => "RegisterJobDefinition",
            "submitjob" => "SubmitJob",
            "submitservicejob" => "SubmitServiceJob",
            "terminatejob" => "TerminateJob",
            "terminateservicejob" => "TerminateServiceJob",
            "updatecomputeenvironment" => "UpdateComputeEnvironment",
            "updateconsumableresource" => "UpdateConsumableResource",
            "updatejobqueue" => "UpdateJobQueue",
            "updatequotashare" => "UpdateQuotaShare",
            "updateschedulingpolicy" => "UpdateSchedulingPolicy",
            "updateserviceenvironment" => "UpdateServiceEnvironment",
            "updateservicejob" => "UpdateServiceJob",
            _ => return None,
        })
    }

    /// Run one consumable-resource / service-environment / quota-share
    /// mutation synchronously through the handler the API dispatches to.
    /// Used by the CloudFormation provisioner, which persists the touched
    /// state itself afterwards.
    pub fn provision_sync(
        &self,
        action: &str,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        match action {
            "CreateConsumableResource"
            | "UpdateConsumableResource"
            | "DeleteConsumableResource"
            | "CreateServiceEnvironment"
            | "UpdateServiceEnvironment"
            | "DeleteServiceEnvironment"
            | "CreateQuotaShare"
            | "UpdateQuotaShare"
            | "DeleteQuotaShare" => self.dispatch(action, req),
            other => Err(AwsServiceError::action_not_implemented("batch", other)),
        }
    }

    fn dispatch(&self, action: &str, req: &AwsRequest) -> Result<AwsResponse, AwsServiceError> {
        match action {
            "CreateComputeEnvironment" => self.create_compute_environment(req),
            "DescribeComputeEnvironments" => self.describe_compute_environments(req),
            "DeleteComputeEnvironment" => self.delete_compute_environment(req),
            "CreateJobQueue" => self.create_job_queue(req),
            "DescribeJobQueues" => self.describe_job_queues(req),
            "DeleteJobQueue" => self.delete_job_queue(req),
            "UpdateComputeEnvironment" => self.update_compute_environment(req),
            "UpdateJobQueue" => self.update_job_queue(req),
            "RegisterJobDefinition" => self.register_job_definition(req),
            "DescribeJobDefinitions" => self.describe_job_definitions(req),
            "DeregisterJobDefinition" => self.deregister_job_definition(req),
            "CreateSchedulingPolicy" => self.create_scheduling_policy(req),
            "DescribeSchedulingPolicies" => self.describe_scheduling_policies(req),
            "ListSchedulingPolicies" => self.list_scheduling_policies(req),
            "UpdateSchedulingPolicy" => self.update_scheduling_policy(req),
            "DeleteSchedulingPolicy" => self.delete_scheduling_policy(req),
            "DescribeJobs" => self.describe_jobs(req),
            "ListJobs" => self.list_jobs(req),
            "TagResource" => self.tag_resource(req),
            "UntagResource" => self.untag_resource(req),
            "ListTagsForResource" => self.list_tags_for_resource(req),
            "CreateConsumableResource" => self.create_consumable_resource(req),
            "DescribeConsumableResource" => self.describe_consumable_resource(req),
            "ListConsumableResources" => self.list_consumable_resources(req),
            "UpdateConsumableResource" => self.update_consumable_resource(req),
            "DeleteConsumableResource" => self.delete_consumable_resource(req),
            "ListJobsByConsumableResource" => self.list_jobs_by_consumable_resource(req),
            "CreateServiceEnvironment" => self.create_service_environment(req),
            "DescribeServiceEnvironments" => self.describe_service_environments(req),
            "UpdateServiceEnvironment" => self.update_service_environment(req),
            "DeleteServiceEnvironment" => self.delete_service_environment(req),
            "CreateQuotaShare" => self.create_quota_share(req),
            "DescribeQuotaShare" => self.describe_quota_share(req),
            "ListQuotaShares" => self.list_quota_shares(req),
            "UpdateQuotaShare" => self.update_quota_share(req),
            "DeleteQuotaShare" => self.delete_quota_share(req),
            "SubmitServiceJob" => self.submit_service_job(req),
            "DescribeServiceJob" => self.describe_service_job(req),
            "ListServiceJobs" => self.list_service_jobs(req),
            "TerminateServiceJob" => self.terminate_service_job(req),
            "UpdateServiceJob" => self.update_service_job(req),
            "GetJobQueueSnapshot" => self.get_job_queue_snapshot(req),
            other => Err(AwsServiceError::action_not_implemented("batch", other)),
        }
    }
}

pub(crate) fn obj(v: &Value) -> Map<String, Value> {
    v.as_object().cloned().unwrap_or_default()
}

pub(crate) fn client_error(code: &str, msg: impl Into<String>) -> AwsServiceError {
    AwsServiceError::aws_error(StatusCode::BAD_REQUEST, code, msg.into())
}

/// Every taggable Batch resource store, with the field holding its ARN.
fn taggable_stores(st: &BatchState) -> [(&crate::state::JsonStore, &str); 9] {
    [
        (&st.compute_environments, "computeEnvironmentArn"),
        (&st.job_queues, "jobQueueArn"),
        (&st.job_definitions, "jobDefinitionArn"),
        (&st.jobs, "jobArn"),
        (&st.scheduling_policies, "arn"),
        (&st.consumable_resources, "consumableResourceArn"),
        (&st.service_environments, "serviceEnvironmentArn"),
        (&st.quota_shares, "quotaShareArn"),
        (&st.service_jobs, "jobArn"),
    ]
}

/// Resolve the `{resourceArn}` label of the tag operations. AWS rejects one
/// that is not a Batch ARN, or names no resource in the account, with
/// `ClientException`, so tags never attach to a resource that does not exist.
fn tagged_resource(
    st: Option<&BatchState>,
    label: Option<&String>,
) -> Result<String, AwsServiceError> {
    let arn = label.ok_or_else(|| client_error("ClientException", "resourceArn is required"))?;
    if fakecloud_aws::arn::arn_resource(arn, "batch").is_none() {
        return Err(client_error(
            "ClientException",
            format!("Invalid resourceArn: {arn}"),
        ));
    }
    let exists = st.is_some_and(|st| {
        taggable_stores(st).iter().any(|(store, key)| {
            store
                .values()
                .any(|r| r.get(*key).and_then(Value::as_str) == Some(arn.as_str()))
        })
    });
    if !exists {
        return Err(client_error(
            "ClientException",
            format!("Resource {arn} does not exist"),
        ));
    }
    Ok(arn.clone())
}

/// `computeEnvironmentOrder` is required on JobQueueDetail; a queue fed by
/// service environments carries it empty rather than absent or null.
pub fn normalize_compute_environment_order(queue: &mut Map<String, Value>) {
    let order = queue
        .entry("computeEnvironmentOrder")
        .or_insert_with(|| json!([]));
    if order.is_null() {
        *order = json!([]);
    }
}

pub(crate) type TagStore =
    std::collections::BTreeMap<String, std::collections::BTreeMap<String, String>>;

/// Seed the authoritative per-ARN tag store from a resource's inline `tags`
/// (what Create receives), so `ListTagsForResource` reflects create-time tags.
pub(crate) fn seed_inline_tags(tags: &mut TagStore, arn: &str, stored: &Map<String, Value>) {
    if let Some(inline) = stored.get("tags").and_then(Value::as_object) {
        let entry = tags.entry(arn.to_string()).or_default();
        for (k, v) in inline {
            if let Some(s) = v.as_str() {
                entry.insert(k.clone(), s.to_string());
            }
        }
    }
}

/// Overlay the authoritative tag store onto a resource on read, so `Describe*`
/// reflects `TagResource`/`UntagResource` (which write only to the store) — the
/// two used to diverge, breaking terraform tag updates.
fn merge_tag_overlay(resource: &Value, arn_key: &str, tags: &TagStore) -> Value {
    let mut o = obj(resource);
    if let Some(arn) = o.get(arn_key).and_then(Value::as_str).map(String::from) {
        if let Some(t) = tags.get(&arn) {
            o.insert(
                "tags".into(),
                Value::Object(t.iter().map(|(k, v)| (k.clone(), json!(v))).collect()),
            );
        }
    }
    Value::Object(o)
}

/// Build a Batch `JobSummary` from a stored job, carrying every AWS-present
/// field that exists on the record (jobId/jobName/createdAt/status are always
/// there; the rest appear once the job has progressed).
fn job_summary(j: &Value) -> Value {
    let mut s = serde_json::Map::new();
    for key in [
        "jobId",
        "jobArn",
        "jobName",
        "createdAt",
        "status",
        "statusReason",
        "startedAt",
        "stoppedAt",
        "jobDefinition",
        "container",
        "arrayProperties",
        "nodeProperties",
    ] {
        if let Some(v) = j.get(key) {
            s.insert(key.to_string(), v.clone());
        }
    }
    Value::Object(s)
}

/// A Batch ARN (`arn:<partition>:batch:<region>:<account>:<resource>`) in
/// `region`'s partition.
pub fn batch_arn(region: &str, account_id: &str, resource: &str) -> String {
    Arn::regional("batch", region, account_id, resource).to_string()
}

impl BatchService {
    // ---- Compute environments ----

    fn create_compute_environment(&self, req: &AwsRequest) -> Result<AwsResponse, AwsServiceError> {
        let body = req.json_body();
        let name = body
            .get("computeEnvironmentName")
            .and_then(Value::as_str)
            .ok_or_else(|| client_error("ClientException", "computeEnvironmentName is required"))?
            .to_string();
        let arn = batch_arn(
            &req.region,
            &req.account_id,
            &format!("compute-environment/{name}"),
        );
        let mut stored = obj(&body);
        stored.insert("computeEnvironmentArn".into(), json!(arn));
        stored.insert("status".into(), json!("VALID"));
        stored.insert("statusReason".into(), json!("ComputeEnvironment Healthy"));
        stored
            .entry("state".to_string())
            .or_insert_with(|| json!("ENABLED"));
        let uuid = Uuid::new_v4().to_string();
        // Managed compute environments are backed by a real ECS cluster; AWS
        // returns its ARN and the provider (and ECS console) read it back.
        stored.insert(
            "ecsClusterArn".into(),
            json!(Arn::regional(
                "ecs",
                &req.region,
                &req.account_id,
                &format!("cluster/AWSBatch-{name}-{uuid}"),
            )
            .to_string()),
        );
        stored.insert("uuid".into(), json!(uuid));

        let mut accounts = self.state.write();
        let st = accounts.get_or_create(&req.account_id);
        if st.compute_environments.contains_key(&name) {
            return Err(client_error(
                "ClientException",
                format!("Object already exists: {name}"),
            ));
        }
        seed_inline_tags(&mut st.tags, &arn, &stored);
        st.compute_environments
            .insert(name.clone(), Value::Object(stored));
        Ok(AwsResponse::ok_json(json!({
            "computeEnvironmentName": name,
            "computeEnvironmentArn": arn,
        })))
    }

    fn describe_compute_environments(
        &self,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        let body = req.json_body();
        let wanted = string_set(&body, "computeEnvironments");
        let accounts = self.state.read();
        let items: Vec<Value> = accounts
            .get(&req.account_id)
            .map(|st| {
                st.compute_environments
                    .values()
                    .filter(|ce| {
                        match_named(
                            ce,
                            &wanted,
                            "computeEnvironmentName",
                            "computeEnvironmentArn",
                        )
                    })
                    .map(|ce| {
                        let mut v = merge_tag_overlay(ce, "computeEnvironmentArn", &st.tags);
                        // AWS always reports the orchestration backend.
                        if let Some(o) = v.as_object_mut() {
                            o.entry("containerOrchestrationType".to_string())
                                .or_insert_with(|| json!("ECS"));
                        }
                        v
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(AwsResponse::ok_json(
            json!({ "computeEnvironments": items }),
        ))
    }

    fn delete_compute_environment(&self, req: &AwsRequest) -> Result<AwsResponse, AwsServiceError> {
        let body = req.json_body();
        let name = arn_or_name(&body, "computeEnvironment")?;
        let mut accounts = self.state.write();
        let st = accounts.get_or_create(&req.account_id);
        // Its tags go with it: a later resource of the same name starts untagged.
        if let Some(arn) = st.compute_environments.remove(&name).and_then(|r| {
            r.get("computeEnvironmentArn")
                .and_then(Value::as_str)
                .map(String::from)
        }) {
            st.tags.remove(&arn);
        }
        Ok(AwsResponse::ok_json(json!({})))
    }

    // ---- Job queues ----

    fn create_job_queue(&self, req: &AwsRequest) -> Result<AwsResponse, AwsServiceError> {
        let body = req.json_body();
        let name = body
            .get("jobQueueName")
            .and_then(Value::as_str)
            .ok_or_else(|| client_error("ClientException", "jobQueueName is required"))?
            .to_string();
        let arn = batch_arn(&req.region, &req.account_id, &format!("job-queue/{name}"));
        let mut stored = obj(&body);
        stored.insert("jobQueueArn".into(), json!(arn));
        stored.insert("status".into(), json!("VALID"));
        stored.insert("statusReason".into(), json!("JobQueue Healthy"));
        // Required on JobQueueDetail: empty for a service-environment queue.
        normalize_compute_environment_order(&mut stored);
        stored
            .entry("state".to_string())
            .or_insert_with(|| json!("ENABLED"));

        let mut accounts = self.state.write();
        let st = accounts.get_or_create(&req.account_id);
        // A service queue's type is the type its service environments share.
        if let Some(kind) = crate::extended::validate_service_environment_order(Some(st), &body)? {
            match stored.get("jobQueueType").and_then(Value::as_str) {
                Some(t) if t != kind => {
                    return Err(client_error(
                        "ClientException",
                        format!(
                            "jobQueueType {t} does not match the service environment type {kind}"
                        ),
                    ));
                }
                Some(_) => {}
                None => {
                    stored.insert("jobQueueType".into(), json!(kind));
                }
            }
        }
        if st.job_queues.contains_key(&name) {
            return Err(client_error(
                "ClientException",
                format!("Object already exists: {name}"),
            ));
        }
        seed_inline_tags(&mut st.tags, &arn, &stored);
        st.job_queues.insert(name.clone(), Value::Object(stored));
        Ok(AwsResponse::ok_json(json!({
            "jobQueueName": name,
            "jobQueueArn": arn,
        })))
    }

    fn describe_job_queues(&self, req: &AwsRequest) -> Result<AwsResponse, AwsServiceError> {
        let body = req.json_body();
        let wanted = string_set(&body, "jobQueues");
        let accounts = self.state.read();
        let items: Vec<Value> = accounts
            .get(&req.account_id)
            .map(|st| {
                st.job_queues
                    .values()
                    .filter(|q| match_named(q, &wanted, "jobQueueName", "jobQueueArn"))
                    .map(|q| {
                        let mut q = merge_tag_overlay(q, "jobQueueArn", &st.tags);
                        // Queues stored before creates normalized the field.
                        if let Some(o) = q.as_object_mut() {
                            normalize_compute_environment_order(o);
                        }
                        q
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(AwsResponse::ok_json(json!({ "jobQueues": items })))
    }

    fn delete_job_queue(&self, req: &AwsRequest) -> Result<AwsResponse, AwsServiceError> {
        let body = req.json_body();
        let name = arn_or_name(&body, "jobQueue")?;
        let mut accounts = self.state.write();
        let st = accounts.get_or_create(&req.account_id);
        // Its tags go with it: a later resource of the same name starts untagged.
        if let Some(arn) = st.job_queues.remove(&name).and_then(|r| {
            r.get("jobQueueArn")
                .and_then(Value::as_str)
                .map(String::from)
        }) {
            st.tags.remove(&arn);
        }
        Ok(AwsResponse::ok_json(json!({})))
    }

    // ---- Job definitions (revisioned) ----

    fn register_job_definition(&self, req: &AwsRequest) -> Result<AwsResponse, AwsServiceError> {
        let body = req.json_body();
        let name = body
            .get("jobDefinitionName")
            .and_then(Value::as_str)
            .ok_or_else(|| client_error("ClientException", "jobDefinitionName is required"))?
            .to_string();
        let job_type = crate::extended::required_str(&body, "type")?;
        if !matches!(job_type, "container" | "multinode") {
            return Err(client_error(
                "ClientException",
                format!(
                    "Invalid job definition type: {job_type}. Valid values: container, multinode"
                ),
            ));
        }
        let mut accounts = self.state.write();
        let st = accounts.get_or_create(&req.account_id);
        let revision = st.job_def_revisions.entry(name.clone()).or_insert(0);
        *revision += 1;
        let revision = *revision;
        let arn = batch_arn(
            &req.region,
            &req.account_id,
            &format!("job-definition/{name}:{revision}"),
        );
        let mut stored = obj(&body);
        stored.insert("jobDefinitionArn".into(), json!(arn));
        stored.insert("revision".into(), json!(revision));
        stored.insert("status".into(), json!("ACTIVE"));
        // AWS defaults the optional list members of containerProperties to empty
        // arrays and echoes them back on describe; clients (the terraform
        // provider) read these back and expect them present.
        if let Some(cp) = stored
            .get_mut("containerProperties")
            .and_then(Value::as_object_mut)
        {
            for key in [
                "environment",
                "mountPoints",
                "resourceRequirements",
                "secrets",
                "ulimits",
                "volumes",
            ] {
                cp.entry(key.to_string()).or_insert_with(|| json!([]));
            }
        }
        seed_inline_tags(&mut st.tags, &arn, &stored);
        st.job_definitions
            .insert(format!("{name}:{revision}"), Value::Object(stored));
        Ok(AwsResponse::ok_json(json!({
            "jobDefinitionName": name,
            "jobDefinitionArn": arn,
            "revision": revision,
        })))
    }

    fn describe_job_definitions(&self, req: &AwsRequest) -> Result<AwsResponse, AwsServiceError> {
        let body = req.json_body();
        let wanted = string_set(&body, "jobDefinitions");
        let name_filter = body
            .get("jobDefinitionName")
            .and_then(Value::as_str)
            .map(|s| s.to_string());
        let status_filter = body
            .get("status")
            .and_then(Value::as_str)
            .map(|s| s.to_string());
        let accounts = self.state.read();
        let items: Vec<Value> = accounts
            .get(&req.account_id)
            .map(|st| {
                st.job_definitions
                    .values()
                    .filter(|jd| {
                        // Match by arn (in `jobDefinitions`), by name, or all.
                        let arn_ok = wanted.is_empty()
                            || jd
                                .get("jobDefinitionArn")
                                .and_then(Value::as_str)
                                .map(|a| wanted.contains(a))
                                .unwrap_or(false)
                            || jd
                                .get("jobDefinitionName")
                                .and_then(Value::as_str)
                                .map(|n| wanted.contains(n))
                                .unwrap_or(false);
                        let name_ok = name_filter.as_deref().is_none_or(|n| {
                            jd.get("jobDefinitionName").and_then(Value::as_str) == Some(n)
                        });
                        let status_ok = status_filter
                            .as_deref()
                            .is_none_or(|s| jd.get("status").and_then(Value::as_str) == Some(s));
                        arn_ok && name_ok && status_ok
                    })
                    .map(|jd| merge_tag_overlay(jd, "jobDefinitionArn", &st.tags))
                    .collect()
            })
            .unwrap_or_default();
        Ok(AwsResponse::ok_json(json!({ "jobDefinitions": items })))
    }

    fn deregister_job_definition(&self, req: &AwsRequest) -> Result<AwsResponse, AwsServiceError> {
        let body = req.json_body();
        let id = body
            .get("jobDefinition")
            .and_then(Value::as_str)
            .ok_or_else(|| client_error("ClientException", "jobDefinition is required"))?;
        // Accept "name:revision" or an ARN ending in name:revision.
        let key = id.rsplit('/').next().unwrap_or(id).to_string();
        let mut accounts = self.state.write();
        let st = accounts.get_or_create(&req.account_id);
        if let Some(jd) = st.job_definitions.get_mut(&key) {
            if let Some(o) = jd.as_object_mut() {
                o.insert("status".into(), json!("INACTIVE"));
            }
        }
        Ok(AwsResponse::ok_json(json!({})))
    }

    fn update_compute_environment(&self, req: &AwsRequest) -> Result<AwsResponse, AwsServiceError> {
        let body = req.json_body();
        let name = arn_or_name(&body, "computeEnvironment")?;
        let mut accounts = self.state.write();
        let st = accounts.get_or_create(&req.account_id);
        let ce = st
            .compute_environments
            .get_mut(&name)
            .ok_or_else(|| client_error("ClientException", format!("Object not found: {name}")))?;
        let arn = merge_updates(
            ce,
            &body,
            &[
                "state",
                "desiredvCpus",
                "computeResources",
                "serviceRole",
                "updatePolicy",
            ],
            "computeEnvironmentArn",
        );
        Ok(AwsResponse::ok_json(json!({
            "computeEnvironmentName": name,
            "computeEnvironmentArn": arn,
        })))
    }

    fn update_job_queue(&self, req: &AwsRequest) -> Result<AwsResponse, AwsServiceError> {
        let body = req.json_body();
        let name = arn_or_name(&body, "jobQueue")?;
        let mut accounts = self.state.write();
        let st = accounts.get_or_create(&req.account_id);
        let stored = st
            .job_queues
            .get(&name)
            .ok_or_else(|| client_error("ClientException", format!("Object not found: {name}")))?;
        let current_type = stored
            .get("jobQueueType")
            .and_then(Value::as_str)
            .map(str::to_string);
        // Validate the queue as it will be after the update, not just the
        // request: a compute queue can't gain a serviceEnvironmentOrder (and
        // vice versa) by sending only one of the two lists.
        let non_empty =
            |v: Option<&Value>| v.and_then(Value::as_array).is_some_and(|a| !a.is_empty());
        let was_service_queue = non_empty(stored.get("serviceEnvironmentOrder"));
        let mut new_type = None;
        let mut clear_type = false;
        if body.get("serviceEnvironmentOrder").is_some()
            || body.get("computeEnvironmentOrder").is_some()
        {
            let pick = |key: &str| {
                body.get(key)
                    .or_else(|| stored.get(key))
                    .cloned()
                    .unwrap_or(Value::Null)
            };
            let effective = json!({
                "computeEnvironmentOrder": pick("computeEnvironmentOrder"),
                "serviceEnvironmentOrder": pick("serviceEnvironmentOrder"),
            });
            let kind = crate::extended::validate_service_environment_order(Some(st), &effective)?;
            match (kind.as_deref(), current_type.as_deref()) {
                (Some(k), Some(c)) if k != c => {
                    return Err(client_error(
                        "ClientException",
                        format!(
                            "Service environment type {k} does not match the job queue type {c}"
                        ),
                    ));
                }
                (Some(k), None) => new_type = Some(k.to_string()),
                // A service queue switched to a compute order is no longer a
                // service queue: drop the service environment type it took.
                (None, Some(_))
                    if was_service_queue && non_empty(effective.get("computeEnvironmentOrder")) =>
                {
                    clear_type = true;
                }
                _ => {}
            }
        }
        let q = st
            .job_queues
            .get_mut(&name)
            .ok_or_else(|| client_error("ClientException", format!("Object not found: {name}")))?;
        if let Some(o) = q.as_object_mut() {
            if let Some(t) = new_type {
                o.insert("jobQueueType".into(), json!(t));
            } else if clear_type {
                o.remove("jobQueueType");
            }
        }
        let arn = merge_updates(
            q,
            &body,
            &[
                "state",
                "priority",
                "computeEnvironmentOrder",
                "serviceEnvironmentOrder",
                "schedulingPolicyArn",
                "jobStateTimeLimitActions",
            ],
            "jobQueueArn",
        );
        if let Some(o) = q.as_object_mut() {
            normalize_compute_environment_order(o);
        }
        Ok(AwsResponse::ok_json(json!({
            "jobQueueName": name,
            "jobQueueArn": arn,
        })))
    }

    // ---- Scheduling policies ----

    fn create_scheduling_policy(&self, req: &AwsRequest) -> Result<AwsResponse, AwsServiceError> {
        let body = req.json_body();
        let name = body
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| client_error("ClientException", "name is required"))?
            .to_string();
        let arn = batch_arn(
            &req.region,
            &req.account_id,
            &format!("scheduling-policy/{name}"),
        );
        let mut stored = obj(&body);
        stored.insert("arn".into(), json!(arn));
        let mut accounts = self.state.write();
        let st = accounts.get_or_create(&req.account_id);
        if st.scheduling_policies.contains_key(&name) {
            return Err(client_error(
                "ClientException",
                format!("Object already exists: {name}"),
            ));
        }
        seed_inline_tags(&mut st.tags, &arn, &stored);
        st.scheduling_policies
            .insert(name.clone(), Value::Object(stored));
        Ok(AwsResponse::ok_json(json!({ "name": name, "arn": arn })))
    }

    fn describe_scheduling_policies(
        &self,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        let body = req.json_body();
        let wanted = string_set(&body, "arns");
        let accounts = self.state.read();
        let items: Vec<Value> = accounts
            .get(&req.account_id)
            .map(|st| {
                st.scheduling_policies
                    .values()
                    .filter(|p| {
                        wanted.is_empty()
                            || p.get("arn")
                                .and_then(Value::as_str)
                                .map(|a| wanted.contains(a))
                                .unwrap_or(false)
                    })
                    .map(|p| merge_tag_overlay(p, "arn", &st.tags))
                    .collect()
            })
            .unwrap_or_default();
        Ok(AwsResponse::ok_json(json!({ "schedulingPolicies": items })))
    }

    fn list_scheduling_policies(&self, req: &AwsRequest) -> Result<AwsResponse, AwsServiceError> {
        let accounts = self.state.read();
        let items: Vec<Value> = accounts
            .get(&req.account_id)
            .map(|st| {
                st.scheduling_policies
                    .values()
                    .filter_map(|p| p.get("arn").map(|a| json!({ "arn": a })))
                    .collect()
            })
            .unwrap_or_default();
        Ok(AwsResponse::ok_json(json!({ "schedulingPolicies": items })))
    }

    fn update_scheduling_policy(&self, req: &AwsRequest) -> Result<AwsResponse, AwsServiceError> {
        let body = req.json_body();
        let name = arn_or_name(&body, "arn")?;
        let mut accounts = self.state.write();
        let st = accounts.get_or_create(&req.account_id);
        let p = st
            .scheduling_policies
            .get_mut(&name)
            .ok_or_else(|| client_error("ClientException", format!("Object not found: {name}")))?;
        merge_updates(p, &body, &["fairsharePolicy", "quotaSharePolicy"], "arn");
        Ok(AwsResponse::ok_json(json!({})))
    }

    fn delete_scheduling_policy(&self, req: &AwsRequest) -> Result<AwsResponse, AwsServiceError> {
        let body = req.json_body();
        let name = arn_or_name(&body, "arn")?;
        let mut accounts = self.state.write();
        let st = accounts.get_or_create(&req.account_id);
        // Its tags go with it: a later resource of the same name starts untagged.
        if let Some(arn) = st
            .scheduling_policies
            .remove(&name)
            .and_then(|r| r.get("arn").and_then(Value::as_str).map(String::from))
        {
            st.tags.remove(&arn);
        }
        Ok(AwsResponse::ok_json(json!({})))
    }

    // ---- Jobs (control plane; real container execution lands in batch 2) ----

    async fn submit_job(&self, req: &AwsRequest) -> Result<AwsResponse, AwsServiceError> {
        let body = req.json_body();
        let job_name = body
            .get("jobName")
            .and_then(Value::as_str)
            .ok_or_else(|| client_error("ClientException", "jobName is required"))?
            .to_string();
        let job_queue = body
            .get("jobQueue")
            .and_then(Value::as_str)
            .ok_or_else(|| client_error("ClientException", "jobQueue is required"))?
            .to_string();
        // AWS rejects SubmitJob against a queue that doesn't exist.
        {
            let queue_name = job_queue.rsplit('/').next().unwrap_or(&job_queue);
            let accounts = self.state.read();
            let exists = accounts.get(&req.account_id).is_some_and(|st| {
                st.job_queues.contains_key(queue_name)
                    || st.job_queues.values().any(|q| {
                        q.get("jobQueueArn").and_then(Value::as_str) == Some(job_queue.as_str())
                    })
            });
            if !exists {
                return Err(client_error(
                    "ClientException",
                    format!("Job queue {job_queue} does not exist"),
                ));
            }
        }
        let job_definition = body
            .get("jobDefinition")
            .and_then(Value::as_str)
            .ok_or_else(|| client_error("ClientException", "jobDefinition is required"))?
            .to_string();
        let job_id = Uuid::new_v4().to_string();
        let arn = batch_arn(&req.region, &req.account_id, &format!("job/{job_id}"));
        let now = chrono::Utc::now().timestamp_millis();
        // AWS Batch caps an array job at 2..=10000 children; reject out-of-range
        // sizes rather than synchronously spawning an unbounded number of
        // container launches (a single-request resource-exhaustion vector).
        let array_size = match body
            .pointer("/arrayProperties/size")
            .and_then(Value::as_i64)
        {
            Some(n) if (2..=10_000).contains(&n) => Some(n),
            Some(n) => {
                return Err(client_error(
                    "ClientException",
                    format!("Array job size must be between 2 and 10000, but was {n}"),
                ));
            }
            None => None,
        };
        let depends_on: Vec<String> = body
            .get("dependsOn")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|d| d.get("jobId").and_then(Value::as_str).map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        // Consumable resources the job needs: the submit-time override, else
        // the job definition's. Every referenced resource must exist.
        let consumables = body
            .get("consumableResourcePropertiesOverride")
            .cloned()
            .or_else(|| {
                self.job_definition_field(
                    &req.account_id,
                    &job_definition,
                    "consumableResourceProperties",
                )
            });
        if let Some(props) = &consumables {
            let accounts = self.state.read();
            crate::extended::validate_job_consumables(accounts.get(&req.account_id), props)?;
        }

        let mut job = obj(&body);
        job.insert("jobId".into(), json!(job_id));
        job.insert("jobArn".into(), json!(arn));
        job.insert("jobName".into(), json!(job_name));
        job.insert("jobQueue".into(), json!(job_queue));
        job.insert("jobDefinition".into(), json!(job_definition));
        job.insert("createdAt".into(), json!(now));
        if let Some(props) = &consumables {
            job.insert("consumableResourceProperties".into(), props.clone());
        }

        // Resolve the job definition's container properties (+ this submit's
        // containerOverrides) so the job runs the right image/command.
        let container = self.resolve_container(&req.account_id, &job_definition, &body);

        if let Some(size) = array_size {
            // Array job: the parent is a tracking record; spawn `size` child
            // jobs `<parent>:<index>`, each with AWS_BATCH_JOB_ARRAY_INDEX set,
            // and run them. The parent's status + statusSummary are computed
            // from the children at DescribeJobs time.
            job.insert("status".into(), json!("PENDING"));
            job.insert("arrayProperties".into(), json!({ "size": size }));
            {
                let mut accounts = self.state.write();
                accounts
                    .get_or_create(&req.account_id)
                    .jobs
                    .insert(job_id.clone(), Value::Object(job));
            }
            for index in 0..size {
                let child_id = format!("{job_id}:{index}");
                let child_arn = batch_arn(&req.region, &req.account_id, &format!("job/{child_id}"));
                let mut child = serde_json::Map::new();
                child.insert("jobId".into(), json!(child_id));
                child.insert("jobArn".into(), json!(child_arn));
                child.insert("jobName".into(), json!(job_name));
                child.insert("jobQueue".into(), json!(job_queue));
                child.insert("jobDefinition".into(), json!(job_definition));
                child.insert("status".into(), json!("SUBMITTED"));
                child.insert("createdAt".into(), json!(now));
                child.insert(
                    "arrayProperties".into(),
                    json!({ "index": index, "statusSummary": {} }),
                );
                if let Some(props) = &consumables {
                    child.insert("consumableResourceProperties".into(), props.clone());
                }
                {
                    let mut accounts = self.state.write();
                    accounts
                        .get_or_create(&req.account_id)
                        .jobs
                        .insert(child_id.clone(), Value::Object(child));
                }
                let child_container = container.clone().map(|c| with_array_index_env(c, index));
                self.launch_job(req, &child_id, &job_name, child_container, now)
                    .await;
            }
        } else if !depends_on.is_empty() {
            // Dependency gating: park at PENDING and spawn a waiter that
            // launches once every dependency has SUCCEEDED (or fails this job if
            // any dependency fails). Never blocks SubmitJob.
            job.insert("status".into(), json!("PENDING"));
            {
                let mut accounts = self.state.write();
                accounts
                    .get_or_create(&req.account_id)
                    .jobs
                    .insert(job_id.clone(), Value::Object(job));
            }
            spawn_dependency_waiter(
                self.launch_ctx(),
                req.account_id.clone(),
                req.region.clone(),
                req.request_id.clone(),
                job_id.clone(),
                job_name.clone(),
                container,
                depends_on,
                now,
            );
        } else {
            job.insert("status".into(), json!("SUBMITTED"));
            {
                let mut accounts = self.state.write();
                accounts
                    .get_or_create(&req.account_id)
                    .jobs
                    .insert(job_id.clone(), Value::Object(job));
            }
            self.launch_job(req, &job_id, &job_name, container, now)
                .await;
        }
        // Launches defer ECS's per-call snapshot; persist the cluster, task
        // definition and every task this request started once, here. The
        // handler's post-mutation snapshot covers the Batch job state.
        if let Some(ecs) = &self.ecs {
            ecs.persist().await;
        }

        Ok(AwsResponse::ok_json(json!({
            "jobArn": arn,
            "jobName": job_name,
            "jobId": job_id,
        })))
    }

    /// Launch a REAL container-backed task on the ECS engine when wired and the
    /// definition carries an image. With no ECS backend (unit tests / no
    /// container runtime) the job stays SUBMITTED honestly — never an
    /// auto-success, which is exactly the rival anti-pattern Batch beats.
    /// Bundle the clones a spawned task needs so the launch path can run with
    /// no `&self` (used by SubmitJob inline and by the dependency waiter).
    fn launch_ctx(&self) -> LaunchCtx {
        LaunchCtx {
            batch_state: self.state.clone(),
            ecs: self.ecs.clone(),
            snapshot_store: self.snapshot_store.clone(),
            snapshot_lock: self.snapshot_lock.clone(),
        }
    }

    async fn launch_job(
        &self,
        req: &AwsRequest,
        job_id: &str,
        job_name: &str,
        container: Option<Value>,
        now: i64,
    ) {
        launch(
            &self.launch_ctx(),
            &req.account_id,
            &req.region,
            &req.request_id,
            job_id,
            job_name,
            container,
            now,
        )
        .await;
    }

    /// Read one top-level field of the job definition a job references
    /// (`name:revision`, bare name -> latest revision, or ARN).
    fn job_definition_field(
        &self,
        account_id: &str,
        job_definition: &str,
        field: &str,
    ) -> Option<Value> {
        let key = job_definition.rsplit('/').next().unwrap_or(job_definition);
        let accounts = self.state.read();
        let st = accounts.get(account_id)?;
        let jd = if key.contains(':') {
            st.job_definitions.get(key)
        } else {
            st.job_definitions
                .iter()
                .filter(|(k, _)| k.rsplit_once(':').map(|(n, _)| n) == Some(key))
                .max_by_key(|(k, _)| {
                    k.rsplit_once(':')
                        .and_then(|(_, r)| r.parse::<i64>().ok())
                        .unwrap_or(0)
                })
                .map(|(_, v)| v)
        }?;
        jd.get(field).filter(|v| !v.is_null()).cloned()
    }

    /// Resolve `containerProperties` from the stored job definition (by
    /// `name:revision`, bare name -> latest, or ARN), then overlay this
    /// submit's `containerOverrides` (command / environment / resourceRequirements).
    pub(crate) fn resolve_container(
        &self,
        account_id: &str,
        job_definition: &str,
        submit_body: &Value,
    ) -> Option<Value> {
        let key = job_definition.rsplit('/').next().unwrap_or(job_definition);
        let accounts = self.state.read();
        let st = accounts.get(account_id)?;
        // Exact "name:revision", else the highest-revision entry for the name.
        let jd = if key.contains(':') {
            st.job_definitions.get(key).cloned()
        } else {
            st.job_definitions
                .iter()
                .filter(|(k, _)| k.rsplit_once(':').map(|(n, _)| n) == Some(key))
                .max_by_key(|(k, _)| {
                    k.rsplit_once(':')
                        .and_then(|(_, r)| r.parse::<i64>().ok())
                        .unwrap_or(0)
                })
                .map(|(_, v)| v.clone())
        }?;
        let mut container = jd.get("containerProperties")?.as_object()?.clone();

        if let Some(ov) = submit_body
            .get("containerOverrides")
            .and_then(Value::as_object)
        {
            for f in ["command", "environment", "resourceRequirements"] {
                if let Some(v) = ov.get(f) {
                    container.insert(f.to_string(), v.clone());
                }
            }
        }
        Some(Value::Object(container))
    }

    fn describe_jobs(&self, req: &AwsRequest) -> Result<AwsResponse, AwsServiceError> {
        let body = req.json_body();
        let wanted = string_set(&body, "jobs");
        let accounts = self.state.read();
        let items: Vec<Value> = accounts
            .get(&req.account_id)
            .map(|st| {
                st.jobs
                    .values()
                    .filter(|j| {
                        wanted.is_empty()
                            || j.get("jobId")
                                .and_then(Value::as_str)
                                .map(|id| wanted.contains(id))
                                .unwrap_or(false)
                    })
                    .map(|j| {
                        // An array parent (arrayProperties.size set, no own ECS
                        // task) reflects its children's aggregate status +
                        // statusSummary computed live.
                        let id = j.get("jobId").and_then(Value::as_str).unwrap_or("");
                        let is_parent = j.pointer("/arrayProperties/size").is_some();
                        if !is_parent {
                            return j.clone();
                        }
                        let prefix = format!("{id}:");
                        let children: Vec<&Value> = st
                            .jobs
                            .iter()
                            .filter(|(k, _)| k.starts_with(&prefix))
                            .map(|(_, v)| v)
                            .collect();
                        let (summary, status) = array_status_summary(&children);
                        let mut out = j.clone();
                        if let Some(o) = out.as_object_mut() {
                            o.insert("status".into(), json!(status));
                            if let Some(ap) =
                                o.get_mut("arrayProperties").and_then(|v| v.as_object_mut())
                            {
                                ap.insert("statusSummary".into(), summary);
                            }
                        }
                        out
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(AwsResponse::ok_json(json!({ "jobs": items })))
    }

    fn list_jobs(&self, req: &AwsRequest) -> Result<AwsResponse, AwsServiceError> {
        let body = req.json_body();
        let queue = body.get("jobQueue").and_then(Value::as_str);
        let array_job_id = body.get("arrayJobId").and_then(Value::as_str);
        let multi_node_job_id = body.get("multiNodeJobId").and_then(Value::as_str);
        // AWS requires exactly one of jobQueue / arrayJobId / multiNodeJobId.
        let selectors = [queue, array_job_id, multi_node_job_id]
            .iter()
            .filter(|s| s.is_some())
            .count();
        if selectors != 1 {
            return Err(client_error(
                "ClientException",
                "The ListJobs request must specify exactly one of jobQueue, arrayJobId, or multiNodeJobId",
            ));
        }
        // "If you don't specify a status, only RUNNING jobs are returned."
        let status = body
            .get("jobStatus")
            .and_then(Value::as_str)
            .unwrap_or("RUNNING")
            .to_string();
        let max_results = body
            .get("maxResults")
            .and_then(Value::as_i64)
            .filter(|n| *n > 0)
            .map(|n| n.min(100) as usize)
            .unwrap_or(100);
        let start: usize = body
            .get("nextToken")
            .and_then(Value::as_str)
            .and_then(|t| t.parse().ok())
            .unwrap_or(0);

        let accounts = self.state.read();
        let mut matched: Vec<&Value> = accounts
            .get(&req.account_id)
            .map(|st| {
                st.jobs
                    .values()
                    .filter(|j| {
                        let selector_ok = if let Some(q) = queue {
                            j.get("jobQueue").and_then(Value::as_str) == Some(q)
                        } else if let Some(a) = array_job_id {
                            j.get("jobId")
                                .and_then(Value::as_str)
                                .is_some_and(|id| id.starts_with(&format!("{a}:")))
                        } else if let Some(m) = multi_node_job_id {
                            j.get("jobId")
                                .and_then(Value::as_str)
                                .is_some_and(|id| id.starts_with(&format!("{m}#")))
                        } else {
                            false
                        };
                        selector_ok
                            && j.get("status").and_then(Value::as_str) == Some(status.as_str())
                    })
                    .collect()
            })
            .unwrap_or_default();
        // Most-recent-first, stable by jobId for ties.
        matched.sort_by(|a, b| {
            let ka = a.get("createdAt").and_then(Value::as_i64).unwrap_or(0);
            let kb = b.get("createdAt").and_then(Value::as_i64).unwrap_or(0);
            kb.cmp(&ka).then_with(|| {
                a.get("jobId")
                    .and_then(Value::as_str)
                    .cmp(&b.get("jobId").and_then(Value::as_str))
            })
        });
        let total = matched.len();
        let items: Vec<Value> = matched
            .into_iter()
            .skip(start)
            .take(max_results)
            .map(job_summary)
            .collect();
        let mut resp = serde_json::Map::new();
        resp.insert("jobSummaryList".into(), Value::Array(items));
        let next = start.saturating_add(max_results);
        if next < total {
            resp.insert("nextToken".into(), json!(next.to_string()));
        }
        Ok(AwsResponse::ok_json(Value::Object(resp)))
    }

    /// `CancelJob` stops jobs that have not started; `TerminateJob` also stops
    /// `STARTING` / `RUNNING` ones. Either applies to an array job's children.
    async fn stop_job(&self, req: &AwsRequest, op: &str) -> Result<AwsResponse, AwsServiceError> {
        let stoppable: &[&str] = if op == "TerminateJob" {
            &["SUBMITTED", "PENDING", "RUNNABLE", "STARTING", "RUNNING"]
        } else {
            &["SUBMITTED", "PENDING", "RUNNABLE"]
        };
        let body = req.json_body();
        let job_id = crate::extended::required_str(&body, "jobId")?.to_string();
        let reason = body
            .get("reason")
            .and_then(Value::as_str)
            .ok_or_else(|| client_error("ClientException", format!("reason is required for {op}")))?
            .to_string();
        let child_prefix = format!("{job_id}:");
        // The real ECS tasks of the jobs being stopped: (cluster, task ARN).
        let mut tasks = Vec::new();
        {
            let mut accounts = self.state.write();
            let jobs = &mut accounts.get_or_create(&req.account_id).jobs;
            for (id, job) in jobs.iter_mut() {
                if *id != job_id && !id.starts_with(&child_prefix) {
                    continue;
                }
                let Some(o) = job.as_object_mut() else {
                    continue;
                };
                let cur = o.get("status").and_then(Value::as_str).unwrap_or("");
                if !stoppable.contains(&cur) {
                    continue;
                }
                o.insert("status".into(), json!("FAILED"));
                o.insert("statusReason".into(), json!(reason));
                o.insert(
                    "stoppedAt".into(),
                    json!(chrono::Utc::now().timestamp_millis()),
                );
                if let (Some(cluster), Some(task)) = (
                    o.get("ecsCluster").and_then(Value::as_str),
                    o.get("ecsTaskArn").and_then(Value::as_str),
                ) {
                    tasks.push((cluster.to_string(), task.to_string()));
                }
            }
        }
        // Stop the containers too, so a terminated job does not keep running.
        // The status poller sees the job already terminal and leaves it FAILED.
        if let (Some(ecs), false) = (self.ecs.as_ref(), tasks.is_empty()) {
            let mut stops = tokio::task::JoinSet::new();
            for (cluster, task) in tasks {
                let ecs = ecs.clone();
                let src = bare_request(&req.account_id, &req.region, &req.request_id);
                let reason = reason.clone();
                stops.spawn(
                    async move { stop_ecs_task(&ecs, &src, &cluster, &task, &reason).await },
                );
            }
            while stops.join_next().await.is_some() {}
            ecs.persist().await;
        }
        Ok(AwsResponse::ok_json(json!({})))
    }

    // ---- Tags ----

    fn tag_resource(&self, req: &AwsRequest) -> Result<AwsResponse, AwsServiceError> {
        let body = req.json_body();
        let tags = body
            .get("tags")
            .and_then(Value::as_object)
            .cloned()
            .ok_or_else(|| client_error("ClientException", "tags is required"))?;
        let mut accounts = self.state.write();
        let st = accounts.get_or_create(&req.account_id);
        let arn = tagged_resource(Some(st), req.path_segments.get(2))?;
        let entry = st.tags.entry(arn).or_default();
        for (k, v) in tags {
            if let Some(s) = v.as_str() {
                entry.insert(k, s.to_string());
            }
        }
        Ok(AwsResponse::ok_json(json!({})))
    }

    fn untag_resource(&self, req: &AwsRequest) -> Result<AwsResponse, AwsServiceError> {
        // `tagKeys` is an `@httpQuery` list sent as repeated `tagKeys=a&tagKeys=b`
        // pairs; `query_params` collapses repeats to the last value, so parse the
        // raw query string for every occurrence, percent-decoding each.
        let keys: Vec<String> = req
            .raw_query
            .split('&')
            .filter_map(|pair| pair.strip_prefix("tagKeys="))
            .map(fakecloud_core::path::percent_decode_segment)
            .collect();
        let mut accounts = self.state.write();
        let st = accounts.get_or_create(&req.account_id);
        let arn = tagged_resource(Some(st), req.path_segments.get(2))?;
        if let Some(entry) = st.tags.get_mut(&arn) {
            for k in keys {
                entry.remove(&k);
            }
        }
        Ok(AwsResponse::ok_json(json!({})))
    }

    fn list_tags_for_resource(&self, req: &AwsRequest) -> Result<AwsResponse, AwsServiceError> {
        let accounts = self.state.read();
        let st = accounts.get(&req.account_id);
        let arn = tagged_resource(st, req.path_segments.get(2))?;
        let tags = st
            .and_then(|st| st.tags.get(&arn))
            .map(|m| {
                m.iter()
                    .map(|(k, v)| (k.clone(), json!(v)))
                    .collect::<Map<String, Value>>()
            })
            .unwrap_or_default();
        Ok(AwsResponse::ok_json(json!({ "tags": tags })))
    }
}

/// Apply the named mutable fields from `body` onto the stored resource and
/// return its ARN (read from `arn_key`).
fn merge_updates(stored: &mut Value, body: &Value, fields: &[&str], arn_key: &str) -> String {
    if let Some(o) = stored.as_object_mut() {
        for f in fields {
            if let Some(v) = body.get(*f) {
                o.insert((*f).to_string(), v.clone());
            }
        }
        o.get(arn_key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    } else {
        String::new()
    }
}

/// Everything the launch path needs without a `&self`, so a spawned task (the
/// dependency waiter) can launch a job exactly like the inline SubmitJob path.
#[derive(Clone)]
struct LaunchCtx {
    batch_state: SharedBatchState,
    ecs: Option<Arc<fakecloud_ecs::EcsService>>,
    snapshot_store: Option<Arc<dyn SnapshotStore>>,
    snapshot_lock: Arc<AsyncMutex<()>>,
}

/// Launch a REAL container-backed task on the ECS engine when the backend is
/// wired and the definition carries an image; otherwise the job stays at its
/// current status honestly (never an auto-success). Updates the Batch job to
/// STARTING + spawns the status sync, or FAILED on a launch error.
#[allow(clippy::too_many_arguments)]
async fn launch(
    ctx: &LaunchCtx,
    account: &str,
    region: &str,
    request_id: &str,
    job_id: &str,
    job_name: &str,
    container: Option<Value>,
    now: i64,
) {
    let (Some(ecs), Some(container)) = (ctx.ecs.clone(), container) else {
        return;
    };
    if container.get("image").and_then(Value::as_str).is_none() {
        return;
    }
    // Consumable resources gate dispatch: a job whose requirements don't fit
    // right now waits at RUNNABLE until capacity is returned.
    let admission = {
        let mut accounts = ctx.batch_state.write();
        crate::extended::admit_job(accounts.get_or_create(account), job_id)
    };
    if let crate::extended::Admission::Stopped = admission {
        return;
    }
    if let crate::extended::Admission::Wait = admission {
        spawn_consumable_waiter(
            ctx.clone(),
            account.to_string(),
            region.to_string(),
            request_id.to_string(),
            job_id.to_string(),
            job_name.to_string(),
            container,
            now,
        );
        return;
    }
    launch_now(
        ctx, ecs, account, region, request_id, job_id, job_name, container, now,
    )
    .await;
}

/// Admit a retrying job's consumable resources, waiting at RUNNABLE until
/// they fit. Returns false when the job left RUNNABLE meanwhile (cancelled,
/// terminated) or is gone, so the retry must not relaunch.
pub(crate) async fn wait_for_retry_admission(
    batch_state: &SharedBatchState,
    account_id: &str,
    job_id: &str,
) -> bool {
    loop {
        {
            let mut accounts = batch_state.write();
            let st = accounts.get_or_create(account_id);
            let runnable = st
                .jobs
                .get(job_id)
                .and_then(|j| j.get("status"))
                .and_then(Value::as_str)
                == Some("RUNNABLE");
            if !runnable {
                return false;
            }
            if let crate::extended::Admission::Admitted = crate::extended::admit_job(st, job_id) {
                return true;
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
}

/// Wait (at RUNNABLE) until a job's consumable resources fit, then launch it.
/// Exits as soon as the job leaves RUNNABLE (cancelled/terminated) or is gone.
#[allow(clippy::too_many_arguments)]
fn spawn_consumable_waiter(
    ctx: LaunchCtx,
    account: String,
    region: String,
    request_id: String,
    job_id: String,
    job_name: String,
    container: Value,
    now: i64,
) {
    let Some(ecs) = ctx.ecs.clone() else {
        return;
    };
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            let admitted = {
                let mut accounts = ctx.batch_state.write();
                let st = accounts.get_or_create(&account);
                let runnable = st
                    .jobs
                    .get(&job_id)
                    .and_then(|j| j.get("status"))
                    .and_then(Value::as_str)
                    == Some("RUNNABLE");
                if !runnable {
                    return;
                }
                matches!(
                    crate::extended::admit_job(st, &job_id),
                    crate::extended::Admission::Admitted
                )
            };
            if admitted {
                launch_now(
                    &ctx,
                    ecs,
                    &account,
                    &region,
                    &request_id,
                    &job_id,
                    &job_name,
                    container,
                    now,
                )
                .await;
                persist_launch(&ctx).await;
                return;
            }
        }
    });
}

/// Launch the job's ECS task now (admission already granted).
#[allow(clippy::too_many_arguments)]
async fn launch_now(
    ctx: &LaunchCtx,
    ecs: Arc<fakecloud_ecs::EcsService>,
    account: &str,
    region: &str,
    request_id: &str,
    job_id: &str,
    job_name: &str,
    container: Value,
    now: i64,
) {
    let src = bare_request(account, region, request_id);
    match launch_ecs_task(&ecs, &src, job_id, job_name, &container).await {
        Ok((cluster, task_arn)) => {
            let recorded = {
                let mut accounts = ctx.batch_state.write();
                match live_job(accounts.get_or_create(account), job_id) {
                    Some(j) => {
                        j.insert("status".into(), json!("STARTING"));
                        j.insert("ecsCluster".into(), json!(cluster));
                        j.insert("ecsTaskArn".into(), json!(task_arn));
                        j.insert("startedAt".into(), json!(now));
                        true
                    }
                    None => false,
                }
            };
            if !recorded {
                // Cancelled or terminated while the task launched: it must not
                // run. The caller persists ECS after the launch.
                stop_ecs_task(&ecs, &src, &cluster, &task_arn, STOPPED_JOB_REASON).await;
                return;
            }
            spawn_status_sync(
                ctx,
                ecs,
                account.to_string(),
                region.to_string(),
                request_id.to_string(),
                job_id.to_string(),
                job_name.to_string(),
                cluster,
                task_arn,
                container,
            );
        }
        Err(err) => {
            let mut accounts = ctx.batch_state.write();
            if let Some(j) = live_job(accounts.get_or_create(account), job_id) {
                j.insert("status".into(), json!("FAILED"));
                j.insert("statusReason".into(), json!(err.message().to_string()));
            }
        }
    }
}

/// Persist both the Batch job state and the ECS resources a launch created.
/// Launches defer ECS's per-call snapshot, so the detached launch paths (the
/// dependency and consumable waiters, retries) call this once afterwards, and
/// SubmitJob calls the ECS half once after launching every child (the
/// handler's own post-mutation snapshot covers the Batch half).
async fn persist_launch(ctx: &LaunchCtx) {
    save_snapshot_now(&ctx.batch_state, &ctx.snapshot_store, &ctx.snapshot_lock).await;
    if let Some(ecs) = &ctx.ecs {
        ecs.persist().await;
    }
}

/// Background waiter for a job with `dependsOn`: poll the dependency job
/// statuses; launch this job once they have all SUCCEEDED, or fail it if any
/// dependency reaches FAILED. Bounded so a never-finishing dependency can't
/// wait forever.
#[allow(clippy::too_many_arguments)]
fn spawn_dependency_waiter(
    ctx: LaunchCtx,
    account: String,
    region: String,
    request_id: String,
    job_id: String,
    job_name: String,
    container: Option<Value>,
    depends_on: Vec<String>,
    now: i64,
) {
    tokio::spawn(async move {
        for _ in 0..1800u32 {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            // Cancelled or terminated while waiting: nothing left to launch.
            if job_is_terminal(&ctx.batch_state, &account, &job_id) {
                return;
            }
            let statuses: Vec<String> = {
                let accounts = ctx.batch_state.read();
                let jobs = accounts.get(&account).map(|s| &s.jobs);
                depends_on
                    .iter()
                    .map(|d| {
                        jobs.and_then(|m| m.get(d))
                            .and_then(|j| j.get("status").and_then(Value::as_str))
                            .unwrap_or("")
                            .to_string()
                    })
                    .collect()
            };
            if statuses.iter().any(|s| s == "FAILED") {
                {
                    let mut accounts = ctx.batch_state.write();
                    if let Some(j) = live_job(accounts.get_or_create(&account), &job_id) {
                        j.insert("status".into(), json!("FAILED"));
                        j.insert("statusReason".into(), json!("Dependent job failed"));
                    }
                }
                save_snapshot_now(&ctx.batch_state, &ctx.snapshot_store, &ctx.snapshot_lock).await;
                return;
            }
            if statuses.iter().all(|s| s == "SUCCEEDED") {
                launch(
                    &ctx,
                    &account,
                    &region,
                    &request_id,
                    &job_id,
                    &job_name,
                    container,
                    now,
                )
                .await;
                // Detached from any handler, so nothing else snapshots the
                // STARTING / FAILED status or the job's ECS resources.
                persist_launch(&ctx).await;
                return;
            }
        }
    });
}

/// Ensure the batch ECS cluster exists, register a task definition from the
/// job's container properties, and RunTask it. Returns (cluster, taskArn).
/// Cross-service calls go through ECS's public `handle()` — reusing all of
/// ECS's real container / portability / k8s handling, not re-deriving it.
async fn launch_ecs_task(
    ecs: &fakecloud_ecs::EcsService,
    src: &AwsRequest,
    job_id: &str,
    job_name: &str,
    container: &Value,
) -> Result<(String, String), AwsServiceError> {
    // Mutations defer ECS's per-call snapshot (an array job launches every
    // child in one request); callers persist once via `EcsService::persist`.
    let cluster = "fakecloud-batch".to_string();
    let described = ecs
        .handle_deferring_snapshot(ecs_request(
            "DescribeClusters",
            json!({ "clusters": [cluster] }),
            src,
        ))
        .await
        .map(|r| parse_body(&r))
        .unwrap_or(Value::Null);
    let active = described
        .get("clusters")
        .and_then(Value::as_array)
        .is_some_and(|cs| {
            cs.iter()
                .any(|c| c.get("status").and_then(Value::as_str) == Some("ACTIVE"))
        });
    if !active {
        let _ = ecs
            .handle_deferring_snapshot(ecs_request(
                "CreateCluster",
                json!({ "clusterName": cluster }),
                src,
            ))
            .await;
    }

    // An array child's index travels as a RunTask container override, as on
    // AWS Batch, so every child of a job shares one task definition revision.
    let (container_def, overrides) = task_definition_container(container);
    let family = format!("batch-{job_name}");
    let task_def_arn =
        match latest_matching_task_definition(ecs, src, &family, &container_def).await {
            Some(arn) => arn,
            None => {
                let reg = ecs
                    .handle_deferring_snapshot(ecs_request(
                        "RegisterTaskDefinition",
                        json!({
                            "family": family,
                            "containerDefinitions": [container_def],
                            "networkMode": "bridge",
                            "requiresCompatibilities": ["EC2"],
                        }),
                        src,
                    ))
                    .await?;
                parse_body(&reg)
                    .pointer("/taskDefinition/taskDefinitionArn")
                    .and_then(Value::as_str)
                    .map(String::from)
                    .unwrap_or(family)
            }
        };

    let mut run_body = json!({
        "cluster": cluster,
        "taskDefinition": task_def_arn,
        "count": 1,
        "launchType": "EC2",
        "startedBy": format!("batch:{job_id}"),
    });
    if let Some(ov) = overrides {
        run_body["overrides"] = json!({ "containerOverrides": [ov] });
    }
    let run = ecs
        .handle_deferring_snapshot(ecs_request("RunTask", run_body, src))
        .await?;
    let run_body: Value = parse_body(&run);
    let task_arn = run_body
        .pointer("/tasks/0/taskArn")
        .and_then(Value::as_str)
        .map(String::from)
        .ok_or_else(|| client_error("ServerException", "RunTask returned no task"))?;
    Ok((cluster, task_arn))
}

/// Split a job's container properties into the ECS container definition the
/// job definition maps to and the per-job RunTask container override (the
/// array index, which differs per child and must not mint a revision each).
fn task_definition_container(container: &Value) -> (Value, Option<Value>) {
    let image = container.get("image").and_then(Value::as_str).unwrap_or("");
    let (vcpus, memory) = container_resources(container);
    let mut cdef = serde_json::Map::new();
    cdef.insert("name".into(), json!("default"));
    cdef.insert("image".into(), json!(image));
    cdef.insert("essential".into(), json!(true));
    cdef.insert("cpu".into(), json!((vcpus * 1024.0).round() as i64));
    cdef.insert("memory".into(), json!(memory));
    if let Some(cmd) = container.get("command").filter(|v| v.is_array()) {
        cdef.insert("command".into(), cmd.clone());
    }
    let mut per_job = Vec::new();
    if let Some(env) = container.get("environment").and_then(Value::as_array) {
        let (index, base): (Vec<Value>, Vec<Value>) = env.iter().cloned().partition(|e| {
            e.get("name").and_then(Value::as_str) == Some("AWS_BATCH_JOB_ARRAY_INDEX")
        });
        per_job = index;
        if !base.is_empty() {
            cdef.insert("environment".into(), Value::Array(base));
        }
    }
    let overrides =
        (!per_job.is_empty()).then(|| json!({ "name": "default", "environment": per_job }));
    (Value::Object(cdef), overrides)
}

/// The ARN of the family's latest ACTIVE revision when its container
/// definition already matches `container_def`, so relaunches (array children,
/// retries, repeat submissions) reuse it instead of minting a new revision.
async fn latest_matching_task_definition(
    ecs: &fakecloud_ecs::EcsService,
    src: &AwsRequest,
    family: &str,
    container_def: &Value,
) -> Option<String> {
    let resp = ecs
        .handle_deferring_snapshot(ecs_request(
            "DescribeTaskDefinition",
            json!({ "taskDefinition": family }),
            src,
        ))
        .await
        .ok()?;
    let td = parse_body(&resp).get("taskDefinition").cloned()?;
    if td.get("status").and_then(Value::as_str) != Some("ACTIVE") {
        return None;
    }
    let existing = td.pointer("/containerDefinitions/0")?;
    let same = ["name", "image", "cpu", "memory", "command", "environment"]
        .iter()
        .all(|k| {
            let want = container_def.get(*k);
            let have = existing.get(*k);
            match (want, have) {
                (None, None) => true,
                (None, Some(v)) => v.as_array().is_some_and(|a| a.is_empty()),
                (Some(w), h) => Some(w) == h,
            }
        });
    if !same {
        return None;
    }
    td.get("taskDefinitionArn")
        .and_then(Value::as_str)
        .map(String::from)
}

/// Poll the ECS task in the background and map its real lifecycle + container
/// exit code onto the Batch job status. NO auto-success: the job only reaches
/// SUCCEEDED when the real container exits 0. Honors the job's `retryStrategy`
/// (re-launch a failed attempt up to `attempts` times) and `timeout`
/// (`attemptDurationSeconds` caps each attempt).
#[allow(clippy::too_many_arguments)]
fn spawn_status_sync(
    ctx: &LaunchCtx,
    ecs: Arc<fakecloud_ecs::EcsService>,
    account_id: String,
    region: String,
    request_id: String,
    job_id: String,
    job_name: String,
    cluster: String,
    task_arn: String,
    container: Value,
) {
    let batch_state = ctx.batch_state.clone();
    let snapshot_store = ctx.snapshot_store.clone();
    let snapshot_lock = ctx.snapshot_lock.clone();
    tokio::spawn(async move {
        let src = bare_request(&account_id, &region, &request_id);
        // retryStrategy.attempts (1-10) and timeout.attemptDurationSeconds.
        let (max_attempts, timeout_secs) = {
            let accounts = batch_state.read();
            let j = accounts.get(&account_id).and_then(|s| s.jobs.get(&job_id));
            let ma = j
                .and_then(|j| j.pointer("/retryStrategy/attempts"))
                .and_then(Value::as_i64)
                .unwrap_or(1)
                .clamp(1, 10);
            let to = j
                .and_then(|j| j.pointer("/timeout/attemptDurationSeconds"))
                .and_then(Value::as_i64)
                .filter(|t| *t > 0);
            (ma, to)
        };
        let max_polls = timeout_secs.unwrap_or(900).min(900) as u32;
        let mut task = task_arn;
        let mut cluster = cluster;
        let mut attempt: i64 = 1;
        loop {
            // Poll this attempt's task until it stops or the timeout elapses.
            let mut outcome: Option<(Option<i64>, Option<String>)> = None; // (exitCode, reason)
            let mut succeeded = false;
            for _ in 0..max_polls {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                if job_is_terminal(&batch_state, &account_id, &job_id) {
                    // Stopped by CancelJob / TerminateJob, which stopped the
                    // task with the caller's reason.
                    return;
                }
                let resp = match ecs
                    .handle(ecs_request(
                        "DescribeTasks",
                        json!({ "cluster": cluster, "tasks": [task] }),
                        &src,
                    ))
                    .await
                {
                    Ok(r) => r,
                    Err(_) => continue,
                };
                let body = parse_body(&resp);
                let Some(t) = body.pointer("/tasks/0") else {
                    continue;
                };
                match t.get("lastStatus").and_then(Value::as_str).unwrap_or("") {
                    "RUNNING" => {
                        let mut accounts = batch_state.write();
                        if let Some(j) = live_job(accounts.get_or_create(&account_id), &job_id) {
                            if matches!(
                                j.get("status").and_then(Value::as_str),
                                Some("STARTING" | "RUNNABLE")
                            ) {
                                j.insert("status".into(), json!("RUNNING"));
                            }
                        }
                    }
                    "STOPPED" => {
                        let code = t.pointer("/containers/0/exitCode").and_then(Value::as_i64);
                        let reason = t
                            .get("stoppedReason")
                            .and_then(Value::as_str)
                            .map(String::from);
                        if code == Some(0) {
                            succeeded = true;
                        } else {
                            outcome =
                                Some((code, reason.or(Some("Essential container exited".into()))));
                        }
                        break;
                    }
                    _ => continue,
                }
            }

            // Cancelled or terminated while this attempt ran: keep that outcome.
            if job_is_terminal(&batch_state, &account_id, &job_id) {
                return;
            }
            if succeeded {
                set_job_terminal(
                    &batch_state,
                    &account_id,
                    &job_id,
                    "SUCCEEDED",
                    Some(0),
                    None,
                );
                save_snapshot_now(&batch_state, &snapshot_store, &snapshot_lock).await;
                break;
            }

            // Failure (or timeout when `outcome` is None).
            let (exit_code, reason) =
                outcome.unwrap_or((None, Some("Job attempt duration exceeded timeout".into())));
            if attempt < max_attempts {
                // Record the failed attempt and re-launch a fresh task.
                {
                    let mut accounts = batch_state.write();
                    if let Some(j) = live_job(accounts.get_or_create(&account_id), &job_id) {
                        let attempts = j.entry("attempts".to_string()).or_insert_with(|| json!([]));
                        if let Some(a) = attempts.as_array_mut() {
                            a.push(json!({
                                "exitCode": exit_code,
                                "statusReason": reason,
                            }));
                        }
                        j.insert("status".into(), json!("RUNNABLE"));
                    }
                }
                save_snapshot_now(&batch_state, &snapshot_store, &snapshot_lock).await;
                // The retry re-enters dispatch: its consumable resources were
                // released when the attempt failed, so it must be admitted
                // again (atomically with the other waiters) before relaunch,
                // and waits at RUNNABLE while capacity is held by others.
                if !wait_for_retry_admission(&batch_state, &account_id, &job_id).await {
                    break;
                }
                let relaunched = launch_ecs_task(&ecs, &src, &job_id, &job_name, &container).await;
                ecs.persist().await;
                match relaunched {
                    Ok((new_cluster, new_task)) => {
                        cluster = new_cluster;
                        task = new_task;
                        attempt += 1;
                        let recorded = {
                            let mut accounts = batch_state.write();
                            match live_job(accounts.get_or_create(&account_id), &job_id) {
                                Some(j) => {
                                    j.insert("status".into(), json!("STARTING"));
                                    j.insert("ecsCluster".into(), json!(cluster));
                                    j.insert("ecsTaskArn".into(), json!(task));
                                    true
                                }
                                None => false,
                            }
                        };
                        if !recorded {
                            // Stopped while this attempt launched: it must not run.
                            stop_ecs_task(&ecs, &src, &cluster, &task, STOPPED_JOB_REASON).await;
                            ecs.persist().await;
                            return;
                        }
                        continue;
                    }
                    Err(_) => {
                        set_job_terminal(
                            &batch_state,
                            &account_id,
                            &job_id,
                            "FAILED",
                            exit_code,
                            reason,
                        );
                        save_snapshot_now(&batch_state, &snapshot_store, &snapshot_lock).await;
                        break;
                    }
                }
            } else {
                set_job_terminal(
                    &batch_state,
                    &account_id,
                    &job_id,
                    "FAILED",
                    exit_code,
                    reason,
                );
                save_snapshot_now(&batch_state, &snapshot_store, &snapshot_lock).await;
                break;
            }
        }
    });
}

/// The ECS stoppedReason for a task stopped because its job already ended.
const STOPPED_JOB_REASON: &str = "Batch job stopped";

/// A job is terminal once `SUCCEEDED` or `FAILED`, whether it finished or was
/// stopped by `CancelJob` / `TerminateJob`. Terminal jobs never change again.
pub(crate) fn is_terminal(job: &Value) -> bool {
    matches!(
        job.get("status").and_then(Value::as_str),
        Some("SUCCEEDED" | "FAILED")
    )
}

/// True once the job is terminal, including through a stop that landed while
/// its task was still being polled.
fn job_is_terminal(batch_state: &SharedBatchState, account_id: &str, job_id: &str) -> bool {
    batch_state
        .read()
        .get(account_id)
        .and_then(|st| st.jobs.get(job_id))
        .is_some_and(is_terminal)
}

/// The job's record while it can still change state, or `None` once it is
/// `SUCCEEDED` / `FAILED`. Every background status writer goes through this
/// under the write lock, so a `CancelJob` / `TerminateJob` (or a finished
/// attempt) is never overwritten by a launch, poll or retry still in flight.
pub(crate) fn live_job<'a>(
    st: &'a mut BatchState,
    job_id: &str,
) -> Option<&'a mut Map<String, Value>> {
    st.jobs
        .get_mut(job_id)
        .filter(|j| !is_terminal(j))
        .and_then(Value::as_object_mut)
}

/// Stop an ECS task Batch launched, recording `reason` as its stoppedReason.
/// Best-effort: a failure is logged. Defers ECS's snapshot like a launch
/// does; the caller persists once.
async fn stop_ecs_task(
    ecs: &fakecloud_ecs::EcsService,
    src: &AwsRequest,
    cluster: &str,
    task: &str,
    reason: &str,
) {
    if let Err(e) = ecs
        .handle_deferring_snapshot(ecs_request(
            "StopTask",
            json!({ "cluster": cluster, "task": task, "reason": reason }),
            src,
        ))
        .await
    {
        tracing::warn!(task, error = %e.message(), "failed to stop the ECS task of a stopped Batch job");
    }
}

/// Write a job's terminal status + `container.exitCode`/reason + `stoppedAt`,
/// unless it is already terminal (a user stop that raced the task's exit wins).
fn set_job_terminal(
    batch_state: &SharedBatchState,
    account_id: &str,
    job_id: &str,
    status: &str,
    exit_code: Option<i64>,
    reason: Option<String>,
) {
    let mut accounts = batch_state.write();
    if let Some(j) = live_job(accounts.get_or_create(account_id), job_id) {
        j.insert("status".into(), json!(status));
        if let Some(c) = exit_code {
            let container = j
                .entry("container".to_string())
                .or_insert_with(|| json!({}));
            if let Some(o) = container.as_object_mut() {
                o.insert("exitCode".into(), json!(c));
            }
        }
        if let Some(r) = reason {
            j.insert("statusReason".into(), json!(r.clone()));
            if let Some(o) = j.get_mut("container").and_then(|v| v.as_object_mut()) {
                o.insert("reason".into(), json!(r));
            }
        }
        j.insert(
            "stoppedAt".into(),
            json!(chrono::Utc::now().timestamp_millis()),
        );
    }
}

/// Build an ECS `AwsRequest` (awsJson1.1) carrying the originating
/// account/region so the launched task lands in the caller's account.
fn ecs_request(action: &str, body: Value, src: &AwsRequest) -> AwsRequest {
    AwsRequest {
        service: "ecs".to_string(),
        action: action.to_string(),
        region: src.region.clone(),
        account_id: src.account_id.clone(),
        request_id: src.request_id.clone(),
        headers: http::HeaderMap::new(),
        query_params: std::collections::HashMap::new(),
        body: bytes::Bytes::from(serde_json::to_vec(&body).unwrap_or_default()),
        body_stream: parking_lot::Mutex::new(None),
        path_segments: Vec::new(),
        raw_path: "/".to_string(),
        raw_query: String::new(),
        method: Method::POST,
        is_query_protocol: false,
        access_key_id: None,
        principal: None,
    }
}

/// A minimal `AwsRequest` for the background poller (carries only the
/// originating account/region/request-id).
fn bare_request(account_id: &str, region: &str, request_id: &str) -> AwsRequest {
    AwsRequest {
        service: "batch".to_string(),
        action: String::new(),
        region: region.to_string(),
        account_id: account_id.to_string(),
        request_id: request_id.to_string(),
        headers: http::HeaderMap::new(),
        query_params: std::collections::HashMap::new(),
        body: bytes::Bytes::new(),
        body_stream: parking_lot::Mutex::new(None),
        path_segments: Vec::new(),
        raw_path: "/".to_string(),
        raw_query: String::new(),
        method: Method::POST,
        is_query_protocol: false,
        access_key_id: None,
        principal: None,
    }
}

/// Parse a JSON `AwsResponse` body, or `Null` on failure.
fn parse_body(resp: &AwsResponse) -> Value {
    serde_json::from_slice(resp.body.expect_bytes()).unwrap_or(Value::Null)
}

/// Resolve (vcpus, memoryMiB) from Batch container properties — either the
/// legacy `vcpus`/`memory` fields or the modern `resourceRequirements` list.
pub(crate) fn container_resources(container: &Value) -> (f64, i64) {
    let mut vcpus = container
        .get("vcpus")
        .and_then(Value::as_f64)
        .unwrap_or(1.0);
    let mut memory = container
        .get("memory")
        .and_then(Value::as_i64)
        .unwrap_or(512);
    if let Some(rr) = container
        .get("resourceRequirements")
        .and_then(Value::as_array)
    {
        for r in rr {
            let ty = r.get("type").and_then(Value::as_str).unwrap_or("");
            let val = r
                .get("value")
                .and_then(Value::as_str)
                .and_then(|s| s.parse::<f64>().ok());
            match (ty, val) {
                ("VCPU", Some(v)) => vcpus = v,
                ("MEMORY", Some(v)) => memory = v as i64,
                _ => {}
            }
        }
    }
    (vcpus.max(0.25), memory.max(4))
}

/// Persist the Batch state now (used by the background status poller, which
/// can't call the `&self` snapshot method).
async fn save_snapshot_now(
    state: &SharedBatchState,
    store: &Option<Arc<dyn SnapshotStore>>,
    lock: &Arc<AsyncMutex<()>>,
) {
    let Some(store) = store.clone() else {
        return;
    };
    let _guard = lock.lock().await;
    let bytes = {
        let snap = BatchSnapshot {
            schema_version: BATCH_SNAPSHOT_SCHEMA_VERSION,
            accounts: Some(state.read().clone()),
        };
        serde_json::to_vec(&snap).unwrap_or_default()
    };
    let _ = tokio::task::spawn_blocking(move || store.save(&bytes)).await;
}

/// Inject `AWS_BATCH_JOB_ARRAY_INDEX=<index>` into a container's environment so
/// each array child can select its slice of work (the AWS Batch contract).
fn with_array_index_env(mut container: Value, index: i64) -> Value {
    if let Some(obj) = container.as_object_mut() {
        let env = obj
            .entry("environment".to_string())
            .or_insert_with(|| json!([]));
        if let Some(arr) = env.as_array_mut() {
            arr.retain(|e| {
                e.get("name").and_then(Value::as_str) != Some("AWS_BATCH_JOB_ARRAY_INDEX")
            });
            arr.push(json!({ "name": "AWS_BATCH_JOB_ARRAY_INDEX", "value": index.to_string() }));
        }
    }
    container
}

/// Aggregate an array parent's child statuses into AWS Batch's
/// `arrayProperties.statusSummary` counts + an overall parent status.
fn array_status_summary(children: &[&Value]) -> (Value, &'static str) {
    let mut summary = serde_json::Map::new();
    for s in [
        "SUBMITTED",
        "PENDING",
        "RUNNABLE",
        "STARTING",
        "RUNNING",
        "SUCCEEDED",
        "FAILED",
    ] {
        let n = children
            .iter()
            .filter(|c| c.get("status").and_then(Value::as_str) == Some(s))
            .count();
        summary.insert(s.to_string(), json!(n));
    }
    let total = children.len();
    let succeeded = summary["SUCCEEDED"].as_u64().unwrap_or(0) as usize;
    let failed = summary["FAILED"].as_u64().unwrap_or(0) as usize;
    let status = if total > 0 && succeeded + failed == total {
        if failed > 0 {
            "FAILED"
        } else {
            "SUCCEEDED"
        }
    } else if summary["RUNNING"].as_u64().unwrap_or(0) > 0
        || summary["STARTING"].as_u64().unwrap_or(0) > 0
    {
        "RUNNING"
    } else {
        "PENDING"
    };
    (Value::Object(summary), status)
}

/// Read a JSON string array into a set of owned strings.
pub(crate) fn string_set(body: &Value, key: &str) -> std::collections::HashSet<String> {
    body.get(key)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// True if `wanted` is empty (match-all) or the resource's name/arn is wanted.
fn match_named(
    res: &Value,
    wanted: &std::collections::HashSet<String>,
    name_key: &str,
    arn_key: &str,
) -> bool {
    if wanted.is_empty() {
        return true;
    }
    let name = res.get(name_key).and_then(Value::as_str);
    let arn = res.get(arn_key).and_then(Value::as_str);
    name.map(|n| wanted.contains(n)).unwrap_or(false)
        || arn.map(|a| wanted.contains(a)).unwrap_or(false)
}

/// Resolve a delete/describe identifier that may be a name or an ARN to the
/// store key (the resource name = last ARN path segment).
pub(crate) fn arn_or_name(body: &Value, key: &str) -> Result<String, AwsServiceError> {
    let raw = body
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| client_error("ClientException", format!("{key} is required")))?;
    Ok(raw.rsplit('/').next().unwrap_or(raw).to_string())
}

#[async_trait]
impl AwsService for BatchService {
    fn service_name(&self) -> &str {
        "batch"
    }

    fn supported_actions(&self) -> &[&str] {
        SUPPORTED_ACTIONS
    }

    async fn handle(&self, req: AwsRequest) -> Result<AwsResponse, AwsServiceError> {
        let Some(action) = Self::resolve_action(&req) else {
            return Err(AwsServiceError::aws_error(
                StatusCode::NOT_FOUND,
                "ResourceNotFoundException",
                format!("Unknown operation: {} {}", req.method, req.raw_path),
            ));
        };
        // SubmitJob launches a real ECS task and Cancel/TerminateJob stop them,
        // so they are async; everything else is a synchronous metadata op.
        let result = match action {
            "SubmitJob" => self.submit_job(&req).await,
            "CancelJob" | "TerminateJob" => self.stop_job(&req, action).await,
            _ => self.dispatch(action, &req),
        };
        if MUTATING_ACTIONS.contains(&action)
            && matches!(result.as_ref(), Ok(resp) if resp.status.is_success())
        {
            self.save_snapshot().await;
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::BatchAccounts;
    use parking_lot::RwLock;
    use std::collections::HashMap;

    fn svc() -> BatchService {
        BatchService::new(Arc::new(RwLock::new(BatchAccounts::new())))
    }

    fn req(path: &str, body: Value) -> AwsRequest {
        let p = path.split('?').next().unwrap_or(path);
        let path_segments = fakecloud_core::path::split_path_segments(p);
        AwsRequest {
            service: "batch".into(),
            action: String::new(),
            region: "us-east-1".into(),
            account_id: "123456789012".into(),
            request_id: "t".into(),
            headers: http::HeaderMap::new(),
            query_params: HashMap::new(),
            body: bytes::Bytes::from(serde_json::to_vec(&body).unwrap()),
            body_stream: parking_lot::Mutex::new(None),
            path_segments,
            raw_path: path.to_string(),
            raw_query: String::new(),
            method: Method::POST,
            is_query_protocol: false,
            access_key_id: None,
            principal: None,
        }
    }

    fn body_of(r: AwsResponse) -> Value {
        serde_json::from_slice(r.body.expect_bytes()).unwrap()
    }

    async fn mk_queue(s: &BatchService, name: &str) {
        s.handle(req(
            "/v1/createjobqueue",
            json!({"jobQueueName": name, "priority": 1}),
        ))
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn compute_environment_lifecycle() {
        let s = svc();
        let r = s
            .handle(req(
                "/v1/createcomputeenvironment",
                json!({"computeEnvironmentName": "ce1", "type": "MANAGED"}),
            ))
            .await
            .unwrap();
        let v = body_of(r);
        assert_eq!(v["computeEnvironmentName"], "ce1");
        assert!(v["computeEnvironmentArn"]
            .as_str()
            .unwrap()
            .contains("compute-environment/ce1"));

        let d = body_of(
            s.handle(req("/v1/describecomputeenvironments", json!({})))
                .await
                .unwrap(),
        );
        let ces = d["computeEnvironments"].as_array().unwrap();
        assert_eq!(ces.len(), 1);
        assert_eq!(ces[0]["status"], "VALID");
        assert_eq!(ces[0]["state"], "ENABLED");

        s.handle(req(
            "/v1/deletecomputeenvironment",
            json!({"computeEnvironment": "ce1"}),
        ))
        .await
        .unwrap();
        let d2 = body_of(
            s.handle(req("/v1/describecomputeenvironments", json!({})))
                .await
                .unwrap(),
        );
        assert_eq!(d2["computeEnvironments"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn china_region_arns_use_the_aws_cn_partition() {
        let s = svc();
        let mut create = req(
            "/v1/createcomputeenvironment",
            json!({"computeEnvironmentName": "ce-cn", "type": "MANAGED"}),
        );
        create.region = "cn-north-1".into();
        let v = body_of(s.handle(create).await.unwrap());
        let arn = v["computeEnvironmentArn"].as_str().unwrap().to_string();
        assert_eq!(
            arn,
            "arn:aws-cn:batch:cn-north-1:123456789012:compute-environment/ce-cn"
        );

        let mut describe = req(
            "/v1/describecomputeenvironments",
            json!({"computeEnvironments": [arn]}),
        );
        describe.region = "cn-north-1".into();
        let d = body_of(s.handle(describe).await.unwrap());
        let ces = d["computeEnvironments"].as_array().unwrap();
        assert_eq!(ces.len(), 1);
        assert!(ces[0]["ecsClusterArn"]
            .as_str()
            .unwrap()
            .starts_with("arn:aws-cn:ecs:cn-north-1:123456789012:cluster/AWSBatch-ce-cn-"));
    }

    #[tokio::test]
    async fn job_definition_revisions_increment() {
        let s = svc();
        for expected in 1..=3 {
            let v = body_of(
                s.handle(req(
                    "/v1/registerjobdefinition",
                    json!({"jobDefinitionName": "jd", "type": "container"}),
                ))
                .await
                .unwrap(),
            );
            assert_eq!(v["revision"], expected);
        }
        let d = body_of(
            s.handle(req(
                "/v1/describejobdefinitions",
                json!({"jobDefinitionName": "jd"}),
            ))
            .await
            .unwrap(),
        );
        assert_eq!(d["jobDefinitions"].as_array().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn every_supported_action_is_dispatched() {
        let s = svc();
        // No supported action may fall through to a not-implemented error: each
        // either works or fails with a modelled ClientException.
        for action in SUPPORTED_ACTIONS {
            if matches!(
                *action,
                "SubmitJob" | "TagResource" | "UntagResource" | "ListTagsForResource"
            ) {
                continue;
            }
            let path = format!("/v1/{}", action.to_ascii_lowercase());
            if let Err(e) = s.handle(req(&path, json!({}))).await {
                assert_ne!(
                    e.status(),
                    StatusCode::NOT_IMPLEMENTED,
                    "{action} is not implemented"
                );
            }
        }
    }

    #[tokio::test]
    async fn required_inputs_are_rejected_when_missing() {
        let s = svc();
        for (path, body) in [
            (
                "/v1/registerjobdefinition",
                json!({"jobDefinitionName": "jd"}),
            ),
            (
                "/v1/registerjobdefinition",
                json!({"jobDefinitionName": "jd", "type": "lambda"}),
            ),
            ("/v1/canceljob", json!({"jobId": "j"})),
            ("/v1/terminatejob", json!({"jobId": "j"})),
        ] {
            match s.handle(req(path, body.clone())).await {
                Err(e) => assert!(format!("{e:?}").contains("ClientException"), "{path}"),
                Ok(_) => panic!("{path} {body} must be rejected"),
            }
        }
        for path in [
            "/v1/tags/",
            "/v1/tags/%7BresourceArn%7D",
            "/v1/tags/not-an-arn",
        ] {
            for method in [Method::GET, Method::POST, Method::DELETE] {
                let mut r = req(path, json!({"tags": {"k": "v"}}));
                r.method = method.clone();
                assert!(
                    s.handle(r).await.is_err(),
                    "{method} {path} has no Batch ARN"
                );
            }
        }
        // Nothing was registered by the rejected calls.
        let d = body_of(
            s.handle(req(
                "/v1/describejobdefinitions",
                json!({"jobDefinitionName": "jd"}),
            ))
            .await
            .unwrap(),
        );
        assert_eq!(d["jobDefinitions"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn tags_attach_only_to_existing_batch_resources() {
        let s = svc();
        let q = body_of(
            s.handle(req(
                "/v1/createjobqueue",
                json!({"jobQueueName": "q", "priority": 1}),
            ))
            .await
            .unwrap(),
        );
        let arn = q["jobQueueArn"].as_str().unwrap().to_string();
        let path = format!("/v1/tags/{}", arn.replace('/', "%2F"));
        let missing = "arn:aws:batch:us-east-1:123456789012:job-queue/nope";
        let missing_path = format!("/v1/tags/{}", missing.replace('/', "%2F"));
        let bad_partition = "/v1/tags/arn:bogus:batch:us-east-1:123456789012:job-queue%2Fq";

        // `tags` is required; a well-formed ARN of nothing, or a non-Batch ARN,
        // is rejected; nothing is recorded for any of them.
        for (p, body) in [
            (path.as_str(), json!({})),
            (missing_path.as_str(), json!({"tags": {"k": "v"}})),
            (bad_partition, json!({"tags": {"k": "v"}})),
        ] {
            match s.handle(req(p, body)).await {
                Err(e) => assert!(format!("{e:?}").contains("ClientException"), "{p}"),
                Ok(_) => panic!("TagResource {p} must be rejected"),
            }
        }
        assert!(!s
            .state
            .read()
            .get("123456789012")
            .unwrap()
            .tags
            .contains_key(missing));

        s.handle(req(&path, json!({"tags": {"k": "v"}})))
            .await
            .unwrap();
        let mut lt = req(&path, json!({}));
        lt.method = Method::GET;
        assert_eq!(body_of(s.handle(lt).await.unwrap())["tags"]["k"], "v");
    }

    #[tokio::test]
    async fn terminating_an_array_job_stops_every_child() {
        let s = svc();
        mk_queue(&s, "q").await;
        let sub = body_of(
            s.handle(req(
                "/v1/submitjob",
                json!({"jobName": "arr", "jobQueue": "q", "jobDefinition": "jd:1",
                       "arrayProperties": {"size": 2}}),
            ))
            .await
            .unwrap(),
        );
        let id = sub["jobId"].as_str().unwrap().to_string();
        s.handle(req(
            "/v1/terminatejob",
            json!({"jobId": id, "reason": "user stop"}),
        ))
        .await
        .unwrap();
        let accounts = s.state.read();
        let jobs = &accounts.get("123456789012").unwrap().jobs;
        for job_id in [id.clone(), format!("{id}:0"), format!("{id}:1")] {
            let j = &jobs[&job_id];
            assert_eq!(j["status"], "FAILED", "{job_id}");
            assert_eq!(j["statusReason"], "user stop", "{job_id}");
            assert!(j["stoppedAt"].is_i64(), "{job_id}");
        }
        drop(accounts);
        // The status poller treats the job as finished and never revives it.
        assert!(job_is_terminal(&s.state, "123456789012", &id));
    }

    #[tokio::test]
    async fn a_stopped_job_is_never_revived_by_background_writers() {
        let s = svc();
        mk_queue(&s, "q").await;
        let sub = body_of(
            s.handle(req(
                "/v1/submitjob",
                json!({"jobName": "j", "jobQueue": "q", "jobDefinition": "jd:1"}),
            ))
            .await
            .unwrap(),
        );
        let id = sub["jobId"].as_str().unwrap().to_string();
        s.handle(req("/v1/canceljob", json!({"jobId": id, "reason": ""})))
            .await
            .unwrap();
        // A container exit racing the cancel does not overwrite it, and a
        // launch waiting on admission (dependsOn, consumables) never starts it.
        set_job_terminal(&s.state, "123456789012", &id, "SUCCEEDED", Some(0), None);
        let mut accounts = s.state.write();
        let st = accounts.get_or_create("123456789012");
        assert!(matches!(
            crate::extended::admit_job(st, &id),
            crate::extended::Admission::Stopped
        ));
        assert!(live_job(st, &id).is_none());
        assert_eq!(st.jobs[&id]["status"], "FAILED");
        assert_eq!(st.jobs[&id]["statusReason"], "");
    }

    #[tokio::test]
    async fn deleting_a_resource_drops_its_tags() {
        let s = svc();
        let make = |name: &str| {
            req(
                "/v1/createjobqueue",
                json!({"jobQueueName": name, "priority": 1, "tags": {"env": "prod"}}),
            )
        };
        let q = body_of(s.handle(make("q")).await.unwrap());
        let arn = q["jobQueueArn"].as_str().unwrap().to_string();
        s.handle(req("/v1/deletejobqueue", json!({"jobQueue": "q"})))
            .await
            .unwrap();
        assert!(!s
            .state
            .read()
            .get("123456789012")
            .unwrap()
            .tags
            .contains_key(&arn));
        // Re-created under the same name (same ARN), it starts untagged.
        s.handle(req(
            "/v1/createjobqueue",
            json!({"jobQueueName": "q", "priority": 1}),
        ))
        .await
        .unwrap();
        let mut lt = req(&format!("/v1/tags/{}", arn.replace('/', "%2F")), json!({}));
        lt.method = Method::GET;
        assert_eq!(body_of(s.handle(lt).await.unwrap())["tags"], json!({}));
    }

    #[tokio::test]
    async fn every_described_queue_carries_compute_environment_order() {
        let s = svc();
        mk_queue(&s, "plain").await;
        let d = body_of(
            s.handle(req("/v1/describejobqueues", json!({})))
                .await
                .unwrap(),
        );
        assert_eq!(d["jobQueues"][0]["computeEnvironmentOrder"], json!([]));
    }

    #[tokio::test]
    async fn tags_agree_across_describe_and_list_tags() {
        let s = svc();
        // Create WITH inline tags.
        let c = body_of(
            s.handle(req(
                "/v1/createcomputeenvironment",
                json!({"computeEnvironmentName": "ce", "type": "MANAGED",
                       "tags": {"team": "data"}}),
            ))
            .await
            .unwrap(),
        );
        let arn = c["computeEnvironmentArn"].as_str().unwrap().to_string();
        let enc = arn.replace('/', "%2F");

        // ListTagsForResource reflects the create-time tags (used to be empty:
        // create wrote them inline, ListTags read a separate store).
        let mut lt = req(&format!("/v1/tags/{enc}"), json!({}));
        lt.method = Method::GET;
        let tags = body_of(s.handle(lt).await.unwrap());
        assert_eq!(tags["tags"]["team"], "data");

        // TagResource adds a tag; Describe* now reflects it (used to diverge).
        let mut tr = req(&format!("/v1/tags/{enc}"), json!({"tags": {"env": "prod"}}));
        tr.method = Method::POST;
        s.handle(tr).await.unwrap();
        let d = body_of(
            s.handle(req("/v1/describecomputeenvironments", json!({})))
                .await
                .unwrap(),
        );
        let ce = &d["computeEnvironments"][0];
        assert_eq!(ce["tags"]["team"], "data");
        assert_eq!(ce["tags"]["env"], "prod");
        // AWS always reports the orchestration backend.
        assert_eq!(ce["containerOrchestrationType"], "ECS");
    }

    #[tokio::test]
    async fn job_submit_describe_cancel_lifecycle() {
        let s = svc();
        mk_queue(&s, "q1").await;
        let sub = body_of(
            s.handle(req(
                "/v1/submitjob",
                json!({"jobName": "j1", "jobQueue": "q1", "jobDefinition": "jd:1"}),
            ))
            .await
            .unwrap(),
        );
        let job_id = sub["jobId"].as_str().unwrap().to_string();
        assert_eq!(sub["jobName"], "j1");

        let d = body_of(
            s.handle(req("/v1/describejobs", json!({"jobs": [job_id]})))
                .await
                .unwrap(),
        );
        assert_eq!(d["jobs"][0]["status"], "SUBMITTED");

        // No jobStatus -> AWS returns only RUNNING (this job is SUBMITTED).
        let running = body_of(
            s.handle(req("/v1/listjobs", json!({"jobQueue": "q1"})))
                .await
                .unwrap(),
        );
        assert_eq!(running["jobSummaryList"].as_array().unwrap().len(), 0);
        // Filtering by the actual status returns it, and the summary carries
        // jobArn (an always-present field the old summary dropped).
        let l = body_of(
            s.handle(req(
                "/v1/listjobs",
                json!({"jobQueue": "q1", "jobStatus": "SUBMITTED"}),
            ))
            .await
            .unwrap(),
        );
        assert_eq!(l["jobSummaryList"].as_array().unwrap().len(), 1);
        assert!(l["jobSummaryList"][0]["jobArn"].as_str().is_some());

        // Listing with no selector is an error (AWS requires exactly one).
        match s.handle(req("/v1/listjobs", json!({}))).await {
            Err(e) => assert!(format!("{e:?}").contains("exactly one")),
            Ok(_) => panic!("ListJobs without a selector must be rejected"),
        }

        s.handle(req(
            "/v1/canceljob",
            json!({"jobId": job_id, "reason": "stop it"}),
        ))
        .await
        .unwrap();
        let d2 = body_of(
            s.handle(req("/v1/describejobs", json!({"jobs": [job_id]})))
                .await
                .unwrap(),
        );
        assert_eq!(d2["jobs"][0]["status"], "FAILED");
        assert_eq!(d2["jobs"][0]["statusReason"], "stop it");
    }

    #[tokio::test]
    async fn scheduling_policy_crud() {
        let s = svc();
        let c = body_of(
            s.handle(req("/v1/createschedulingpolicy", json!({"name": "sp1"})))
                .await
                .unwrap(),
        );
        let arn = c["arn"].as_str().unwrap().to_string();
        assert!(arn.contains("scheduling-policy/sp1"));
        let d = body_of(
            s.handle(req(
                "/v1/describeschedulingpolicies",
                json!({"arns": [arn]}),
            ))
            .await
            .unwrap(),
        );
        assert_eq!(d["schedulingPolicies"].as_array().unwrap().len(), 1);
        s.handle(req("/v1/deleteschedulingpolicy", json!({"arn": "sp1"})))
            .await
            .unwrap();
        let l = body_of(
            s.handle(req("/v1/listschedulingpolicies", json!({})))
                .await
                .unwrap(),
        );
        assert_eq!(l["schedulingPolicies"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn update_compute_environment_persists() {
        let s = svc();
        s.handle(req(
            "/v1/createcomputeenvironment",
            json!({"computeEnvironmentName": "ce1", "state": "ENABLED"}),
        ))
        .await
        .unwrap();
        s.handle(req(
            "/v1/updatecomputeenvironment",
            json!({"computeEnvironment": "ce1", "state": "DISABLED"}),
        ))
        .await
        .unwrap();
        let d = body_of(
            s.handle(req("/v1/describecomputeenvironments", json!({})))
                .await
                .unwrap(),
        );
        assert_eq!(d["computeEnvironments"][0]["state"], "DISABLED");
    }

    #[test]
    fn container_resources_from_both_shapes() {
        // Legacy vcpus/memory.
        let (v, m) = container_resources(&json!({"vcpus": 2, "memory": 2048}));
        assert_eq!(v, 2.0);
        assert_eq!(m, 2048);
        // Modern resourceRequirements override.
        let (v, m) = container_resources(&json!({
            "resourceRequirements": [
                {"type": "VCPU", "value": "4"},
                {"type": "MEMORY", "value": "8192"}
            ]
        }));
        assert_eq!(v, 4.0);
        assert_eq!(m, 8192);
        // Defaults + floors.
        let (v, m) = container_resources(&json!({}));
        assert_eq!(v, 1.0);
        assert_eq!(m, 512);
    }

    #[tokio::test]
    async fn resolve_container_picks_latest_revision_and_overrides() {
        let s = svc();
        for cmd in [json!(["echo", "v1"]), json!(["echo", "v2"])] {
            s.handle(req(
                "/v1/registerjobdefinition",
                json!({
                    "jobDefinitionName": "jd",
                    "type": "container",
                    "containerProperties": {"image": "alpine", "command": cmd}
                }),
            ))
            .await
            .unwrap();
        }
        // Bare name -> latest revision (v2).
        let c = s
            .resolve_container("123456789012", "jd", &json!({}))
            .unwrap();
        assert_eq!(c["command"], json!(["echo", "v2"]));
        // containerOverrides win.
        let c = s
            .resolve_container(
                "123456789012",
                "jd:1",
                &json!({"containerOverrides": {"command": ["overridden"]}}),
            )
            .unwrap();
        assert_eq!(c["command"], json!(["overridden"]));
        assert_eq!(c["image"], "alpine");
    }

    #[tokio::test]
    async fn submit_without_ecs_parks_at_submitted_never_auto_succeeds() {
        // No ECS backend wired: the job must stay SUBMITTED (honest), not
        // fake a SUCCEEDED like the rivals do.
        let s = svc();
        mk_queue(&s, "q").await;
        s.handle(req(
            "/v1/registerjobdefinition",
            json!({"jobDefinitionName": "jd", "type": "container",
                   "containerProperties": {"image": "alpine"}}),
        ))
        .await
        .unwrap();
        let sub = body_of(
            s.handle(req(
                "/v1/submitjob",
                json!({"jobName": "j", "jobQueue": "q", "jobDefinition": "jd"}),
            ))
            .await
            .unwrap(),
        );
        let id = sub["jobId"].as_str().unwrap().to_string();
        let d = body_of(
            s.handle(req("/v1/describejobs", json!({"jobs": [id]})))
                .await
                .unwrap(),
        );
        assert_eq!(d["jobs"][0]["status"], "SUBMITTED");
    }

    #[derive(Default)]
    struct CapturingStore {
        saves: parking_lot::Mutex<Vec<Vec<u8>>>,
    }

    impl SnapshotStore for CapturingStore {
        fn load(&self) -> std::io::Result<Option<Vec<u8>>> {
            Ok(self.saves.lock().last().cloned())
        }

        fn save(&self, bytes: &[u8]) -> std::io::Result<()> {
            self.saves.lock().push(bytes.to_vec());
            Ok(())
        }
    }

    #[tokio::test]
    async fn launch_persists_ecs_resources_and_job_status() {
        // The job's ECS cluster / task definition / task must go through the
        // wired ECS service's snapshot store (else they vanish on restart and
        // ecsTaskArn dangles), and the detached launch path must persist the
        // Batch job's resulting status itself.
        let ecs_store = Arc::new(CapturingStore::default());
        let ecs_state: fakecloud_ecs::SharedEcsState = Arc::new(RwLock::new(
            fakecloud_core::multi_account::MultiAccountState::new(
                "123456789012",
                "us-east-1",
                "http://localhost:4566",
            ),
        ));
        let ecs = Arc::new(
            fakecloud_ecs::EcsService::new(ecs_state)
                .with_snapshot_store(ecs_store.clone() as Arc<dyn SnapshotStore>),
        );
        let batch_store = Arc::new(CapturingStore::default());
        let s = svc()
            .with_snapshot_store(batch_store.clone() as Arc<dyn SnapshotStore>)
            .with_ecs(ecs);
        s.state.write().get_or_create("123456789012").jobs.insert(
            "job-1".into(),
            json!({"jobId": "job-1", "status": "SUBMITTED"}),
        );

        // As the detached launch paths (dependency / consumable waiters) do.
        let ctx = s.launch_ctx();
        launch(
            &ctx,
            "123456789012",
            "us-east-1",
            "t",
            "job-1",
            "persisted",
            Some(json!({"image": "alpine", "vcpus": 1, "memory": 128})),
            0,
        )
        .await;
        persist_launch(&ctx).await;

        let ecs_saved = String::from_utf8(ecs_store.saves.lock().last().cloned().unwrap()).unwrap();
        assert!(
            ecs_saved.contains("fakecloud-batch"),
            "cluster not persisted"
        );
        assert!(
            ecs_saved.contains("batch-persisted"),
            "task definition not persisted"
        );

        let status = s.state.read().get("123456789012").unwrap().jobs["job-1"]["status"]
            .as_str()
            .unwrap()
            .to_string();
        assert_ne!(status, "SUBMITTED");
        let batch_saved: Value =
            serde_json::from_slice(&batch_store.saves.lock().last().cloned().unwrap()).unwrap();
        assert_eq!(
            batch_saved.pointer("/accounts/accounts/123456789012/jobs/job-1/status"),
            Some(&json!(status)),
            "job status not snapshotted: {batch_saved}"
        );
    }

    /// An array job launches every child inside one SubmitJob: the children
    /// share one task definition revision (the index rides on a RunTask
    /// container override) and ECS is snapshotted once for the whole request,
    /// not several times per child.
    #[tokio::test]
    async fn array_submit_shares_one_revision_and_snapshots_ecs_once() {
        let ecs_store = Arc::new(CapturingStore::default());
        let ecs_state: fakecloud_ecs::SharedEcsState = Arc::new(RwLock::new(
            fakecloud_core::multi_account::MultiAccountState::new(
                "123456789012",
                "us-east-1",
                "http://localhost:4566",
            ),
        ));
        let ecs = Arc::new(
            fakecloud_ecs::EcsService::new(ecs_state)
                .with_snapshot_store(ecs_store.clone() as Arc<dyn SnapshotStore>),
        );
        let s = svc().with_ecs(ecs.clone());
        mk_queue(&s, "q").await;
        s.handle(req(
            "/v1/registerjobdefinition",
            json!({"jobDefinitionName": "jd", "type": "container",
                   "containerProperties": {"image": "alpine", "vcpus": 1, "memory": 128,
                       "environment": [{"name": "MODE", "value": "batch"}]}}),
        ))
        .await
        .unwrap();
        let before = ecs_store.saves.lock().len();
        let sub = body_of(
            s.handle(req(
                "/v1/submitjob",
                json!({"jobName": "arr", "jobQueue": "q", "jobDefinition": "jd",
                       "arrayProperties": {"size": 4}}),
            ))
            .await
            .unwrap(),
        );
        assert!(sub["jobId"].is_string());
        assert_eq!(
            ecs_store.saves.lock().len() - before,
            1,
            "one ECS snapshot for the whole array submit"
        );

        let src = bare_request("123456789012", "us-east-1", "t");
        let call = |action: &str, body: Value| ecs.handle(ecs_request(action, body, &src));
        let defs = parse_body(
            &call("ListTaskDefinitions", json!({"familyPrefix": "batch-arr"}))
                .await
                .unwrap(),
        );
        assert_eq!(
            defs["taskDefinitionArns"].as_array().unwrap().len(),
            1,
            "children share one revision: {defs}"
        );
        let tasks = parse_body(
            &call("ListTasks", json!({"cluster": "fakecloud-batch"}))
                .await
                .unwrap(),
        );
        let arns = tasks["taskArns"].as_array().unwrap().clone();
        assert_eq!(arns.len(), 4, "{tasks}");
        let described = parse_body(
            &call(
                "DescribeTasks",
                json!({"cluster": "fakecloud-batch", "tasks": arns}),
            )
            .await
            .unwrap(),
        );
        let mut indexes: Vec<String> = described["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| {
                let env = &t["overrides"]["containerOverrides"][0]["environment"];
                assert_eq!(env[0]["name"], "AWS_BATCH_JOB_ARRAY_INDEX", "{t}");
                env[0]["value"].as_str().unwrap().to_string()
            })
            .collect();
        indexes.sort();
        assert_eq!(indexes, ["0", "1", "2", "3"]);

        // A later submit of the same definition reuses the revision too.
        s.handle(req(
            "/v1/submitjob",
            json!({"jobName": "arr", "jobQueue": "q", "jobDefinition": "jd"}),
        ))
        .await
        .unwrap();
        let defs = parse_body(
            &call("ListTaskDefinitions", json!({"familyPrefix": "batch-arr"}))
                .await
                .unwrap(),
        );
        assert_eq!(
            defs["taskDefinitionArns"].as_array().unwrap().len(),
            1,
            "{defs}"
        );
    }

    #[tokio::test]
    async fn reconcile_fails_in_flight_jobs_after_restart() {
        // Simulate a restart: a snapshot restored a job in a non-terminal state
        // whose background driver no longer exists. Reconcile must fail it
        // (with a clear reason) rather than leave it frozen forever.
        let s = svc();
        mk_queue(&s, "q").await;
        s.handle(req(
            "/v1/registerjobdefinition",
            json!({"jobDefinitionName": "jd", "type": "container",
                   "containerProperties": {"image": "alpine"}}),
        ))
        .await
        .unwrap();
        let sub = body_of(
            s.handle(req(
                "/v1/submitjob",
                json!({"jobName": "j", "jobQueue": "q", "jobDefinition": "jd"}),
            ))
            .await
            .unwrap(),
        );
        let id = sub["jobId"].as_str().unwrap().to_string();

        s.reconcile_persisted_jobs().await;

        let d = body_of(
            s.handle(req("/v1/describejobs", json!({"jobs": [id]})))
                .await
                .unwrap(),
        );
        assert_eq!(d["jobs"][0]["status"], "FAILED");
        assert_eq!(
            d["jobs"][0]["statusReason"],
            "Job interrupted by a fakecloud restart"
        );
        assert!(d["jobs"][0]["stoppedAt"].is_i64());

        // A second reconcile is a no-op: already-terminal jobs are untouched.
        s.reconcile_persisted_jobs().await;
        let d2 = body_of(
            s.handle(req("/v1/describejobs", json!({"jobs": [id]})))
                .await
                .unwrap(),
        );
        assert_eq!(d2["jobs"][0]["status"], "FAILED");
    }

    #[test]
    fn array_index_env_injected() {
        let c = with_array_index_env(json!({"image": "alpine"}), 5);
        let env = c["environment"].as_array().unwrap();
        assert!(env
            .iter()
            .any(|e| e["name"] == "AWS_BATCH_JOB_ARRAY_INDEX" && e["value"] == "5"));
    }

    #[tokio::test]
    async fn array_job_spawns_children_and_parent_aggregates() {
        let s = svc();
        mk_queue(&s, "q").await;
        s.handle(req(
            "/v1/registerjobdefinition",
            json!({"jobDefinitionName": "jd", "type": "container",
                   "containerProperties": {"image": "alpine"}}),
        ))
        .await
        .unwrap();
        let sub = body_of(
            s.handle(req(
                "/v1/submitjob",
                json!({"jobName": "arr", "jobQueue": "q", "jobDefinition": "jd",
                       "arrayProperties": {"size": 3}}),
            ))
            .await
            .unwrap(),
        );
        let parent = sub["jobId"].as_str().unwrap().to_string();

        // The arrayJobId selector returns exactly the 3 children (no ECS wired
        // -> they stay SUBMITTED honestly, so filter by that status).
        let listed = body_of(
            s.handle(req(
                "/v1/listjobs",
                json!({"arrayJobId": parent, "jobStatus": "SUBMITTED"}),
            ))
            .await
            .unwrap(),
        );
        let children = listed["jobSummaryList"].as_array().unwrap();
        assert_eq!(children.len(), 3);
        assert!(children.iter().all(|j| j["jobId"]
            .as_str()
            .unwrap()
            .starts_with(&format!("{parent}:"))));

        // The parent aggregates: PENDING with statusSummary SUBMITTED=3.
        let d = body_of(
            s.handle(req("/v1/describejobs", json!({"jobs": [parent]})))
                .await
                .unwrap(),
        );
        let p = &d["jobs"][0];
        assert_eq!(p["status"], "PENDING");
        assert_eq!(p["arrayProperties"]["statusSummary"]["SUBMITTED"], 3);
        assert_eq!(p["arrayProperties"]["size"], 3);
    }

    #[test]
    fn array_summary_terminal_states() {
        let succ = json!({"status": "SUCCEEDED"});
        let fail = json!({"status": "FAILED"});
        let run = json!({"status": "RUNNING"});
        let (_, st) = array_status_summary(&[&succ, &succ]);
        assert_eq!(st, "SUCCEEDED");
        let (_, st) = array_status_summary(&[&succ, &fail]);
        assert_eq!(st, "FAILED");
        let (_, st) = array_status_summary(&[&succ, &run]);
        assert_eq!(st, "RUNNING");
    }

    #[tokio::test]
    async fn depends_on_parks_at_pending() {
        let s = svc();
        mk_queue(&s, "q").await;
        s.handle(req(
            "/v1/registerjobdefinition",
            json!({"jobDefinitionName": "jd", "type": "container",
                   "containerProperties": {"image": "alpine"}}),
        ))
        .await
        .unwrap();
        let a = body_of(
            s.handle(req(
                "/v1/submitjob",
                json!({"jobName": "a", "jobQueue": "q", "jobDefinition": "jd"}),
            ))
            .await
            .unwrap(),
        );
        let a_id = a["jobId"].as_str().unwrap().to_string();
        let b = body_of(
            s.handle(req(
                "/v1/submitjob",
                json!({"jobName": "b", "jobQueue": "q", "jobDefinition": "jd",
                       "dependsOn": [{"jobId": a_id, "type": "SEQUENTIAL"}]}),
            ))
            .await
            .unwrap(),
        );
        // B parks at PENDING (its dep A is only SUBMITTED, never SUCCEEDED
        // without a runtime) — it must not launch ahead of its dependency.
        let d = body_of(
            s.handle(req(
                "/v1/describejobs",
                json!({"jobs": [b["jobId"].clone()]}),
            ))
            .await
            .unwrap(),
        );
        assert_eq!(d["jobs"][0]["status"], "PENDING");
    }

    #[test]
    fn routes_tag_family_by_method() {
        let mut r = req("/v1/tags/arn%3Aaws", json!({}));
        r.method = Method::GET;
        assert_eq!(
            BatchService::resolve_action(&r),
            Some("ListTagsForResource")
        );
        r.method = Method::DELETE;
        assert_eq!(BatchService::resolve_action(&r), Some("UntagResource"));
    }

    #[tokio::test]
    async fn list_tags_with_multibyte_arn_does_not_panic() {
        let s = svc();
        // Raw multi-byte byte in the ARN path segment (what dispatch would pass
        // through unescaped) must yield a normal ClientException (it is not a
        // Batch ARN), not a dropped conn.
        let mut r = req("/v1/tags/%€", json!({}));
        r.method = Method::GET;
        match s.handle(r).await {
            Err(e) => assert!(format!("{e:?}").contains("ClientException")),
            Ok(_) => panic!("a non-ARN resourceArn must be rejected"),
        }
    }

    #[tokio::test]
    async fn submit_job_rejects_out_of_range_array_size() {
        let s = svc();
        mk_queue(&s, "q").await;
        for bad in [1_i64, 0, -3, 10_001, 2_000_000_000] {
            let res = s
                .handle(req(
                    "/v1/submitjob",
                    json!({
                        "jobName": "j", "jobQueue": "q", "jobDefinition": "d",
                        "arrayProperties": {"size": bad}
                    }),
                ))
                .await;
            match res {
                Err(e) => assert!(
                    format!("{e:?}").contains("between 2 and 10000"),
                    "size {bad}: wrong error {e:?}"
                ),
                Ok(_) => panic!("size {bad}: out-of-range array size must be rejected"),
            }
        }
    }
}
