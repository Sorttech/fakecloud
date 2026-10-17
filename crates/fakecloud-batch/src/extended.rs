//! Consumable resources, service environments, quota shares, service jobs and
//! the job-queue snapshot.
//!
//! Service jobs are SageMaker Training jobs queued through Batch. fakecloud's
//! SageMaker is control-plane only (no training executor), so a submitted
//! service job is accepted, validated against its queue / quota share /
//! fair-share rules and parked at `RUNNABLE` (waiting for dispatch) until it is
//! terminated. It never fabricates a SageMaker training job or a success.

use std::collections::BTreeMap;

use serde_json::{json, Map, Value};
use uuid::Uuid;

use fakecloud_core::service::{AwsRequest, AwsResponse, AwsServiceError};

use crate::service::{
    batch_arn, client_error, container_resources, obj, seed_inline_tags, string_set, BatchService,
    TagStore,
};
use crate::state::BatchState;

/// Statuses in which a container job holds its consumable resources.
const HOLDING: &[&str] = &["STARTING", "RUNNING"];
/// Service-job statuses that have not reached an end state.
const SERVICE_JOB_ACTIVE: &[&str] = &[
    "SUBMITTED",
    "PENDING",
    "RUNNABLE",
    "SCHEDULED",
    "STARTING",
    "RUNNING",
];
/// clientToken validity window for `UpdateConsumableResource` (8 hours).
const CLIENT_TOKEN_TTL_MS: i64 = 8 * 60 * 60 * 1000;

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

pub(crate) fn required_str<'a>(body: &'a Value, key: &str) -> Result<&'a str, AwsServiceError> {
    body.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| client_error("ClientException", format!("{key} is required")))
}

/// Batch resource names: up to 128 letters, numbers, hyphens and underscores.
fn validate_name(field: &str, name: &str) -> Result<(), AwsServiceError> {
    let ok = !name.is_empty()
        && name.len() <= 128
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if ok {
        Ok(())
    } else {
        Err(client_error(
            "ClientException",
            format!(
                "{field} must be 1-128 characters and contain only letters, numbers, hyphens (-), and underscores (_)"
            ),
        ))
    }
}

fn validate_enum(
    field: &str,
    value: Option<&Value>,
    allowed: &[&str],
) -> Result<(), AwsServiceError> {
    match value {
        None | Some(Value::Null) => Ok(()),
        Some(v) => match v.as_str() {
            Some(s) if allowed.contains(&s) => Ok(()),
            _ => Err(client_error(
                "ClientException",
                format!("{field} must be one of: {}", allowed.join(", ")),
            )),
        },
    }
}

/// Case-insensitive name match; a trailing `*` makes it a prefix match.
fn name_filter_matches(pattern: &str, name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    let pattern = pattern.to_ascii_lowercase();
    match pattern.strip_suffix('*') {
        Some(prefix) => name.starts_with(prefix),
        None => name == pattern,
    }
}

/// `filters: [{name, values}]` -> name -> values.
fn parse_filters(body: &Value) -> Vec<(String, Vec<String>)> {
    body.get("filters")
        .and_then(Value::as_array)
        .map(|fs| {
            fs.iter()
                .filter_map(|f| {
                    let name = f.get("name").and_then(Value::as_str)?.to_string();
                    let values = f
                        .get("values")
                        .and_then(Value::as_array)
                        .map(|vs| {
                            vs.iter()
                                .filter_map(|v| v.as_str().map(String::from))
                                .collect()
                        })
                        .unwrap_or_default();
                    Some((name, values))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Apply `maxResults` (1-100, default 100) / `nextToken` (an offset) paging.
fn paginate(
    items: Vec<Value>,
    body: &Value,
) -> Result<(Vec<Value>, Option<String>), AwsServiceError> {
    let max_results = match body.get("maxResults").and_then(Value::as_i64) {
        None => 100,
        Some(n) if (1..=100).contains(&n) => n as usize,
        Some(n) => {
            return Err(client_error(
                "ClientException",
                format!("maxResults must be between 1 and 100, but was {n}"),
            ))
        }
    };
    let start = match body.get("nextToken").and_then(Value::as_str) {
        None => 0,
        Some(t) => t
            .parse::<usize>()
            .map_err(|_| client_error("ClientException", "Invalid nextToken"))?,
    };
    let total = items.len();
    let page: Vec<Value> = items.into_iter().skip(start).take(max_results).collect();
    let next = start.saturating_add(max_results);
    Ok((page, (next < total).then(|| next.to_string())))
}

fn tags_value(tags: &TagStore, arn: &str) -> Option<Value> {
    tags.get(arn)
        .filter(|t| !t.is_empty())
        .map(|t| Value::Object(t.iter().map(|(k, v)| (k.clone(), json!(v))).collect()))
}

/// Copy the listed keys (when present) from `src` into a fresh object.
fn project(src: &Value, keys: &[&str]) -> Map<String, Value> {
    let mut out = Map::new();
    for k in keys {
        if let Some(v) = src.get(*k).filter(|v| !v.is_null()) {
            out.insert((*k).to_string(), v.clone());
        }
    }
    out
}

/// Find a job queue by name or ARN.
pub(crate) fn find_queue<'a>(st: &'a BatchState, id: &str) -> Option<&'a Value> {
    let name = id.rsplit('/').next().unwrap_or(id);
    st.job_queues.get(name).or_else(|| {
        st.job_queues
            .values()
            .find(|q| q.get("jobQueueArn").and_then(Value::as_str) == Some(id))
    })
}

/// Resolve a consumable resource reference (name or ARN) to its store key.
fn consumable_key(st: &BatchState, id: &str) -> Option<String> {
    let name = id.rsplit('/').next().unwrap_or(id);
    if st.consumable_resources.contains_key(name) {
        return Some(name.to_string());
    }
    st.consumable_resources
        .iter()
        .find(|(_, r)| r.get("consumableResourceArn").and_then(Value::as_str) == Some(id))
        .map(|(k, _)| k.clone())
}

/// The `consumableResourceList` entries a job requires, as (reference, quantity).
pub(crate) fn job_consumable_requirements(job: &Value) -> Vec<(String, i64)> {
    job.pointer("/consumableResourceProperties/consumableResourceList")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|r| {
                    let id = r.get("consumableResource").and_then(Value::as_str)?;
                    let qty = r.get("quantity").and_then(Value::as_i64).unwrap_or(0);
                    Some((id.to_string(), qty))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// True when a job's requirement reference names this resource.
fn references(reference: &str, name: &str, arn: &str) -> bool {
    reference == arn || reference.rsplit('/').next() == Some(name)
}

/// Quantity of a consumable resource currently consumed by jobs. A
/// replenishable resource is held while a job is STARTING/RUNNING and returned
/// when it finishes; a non-replenishable one stays consumed once a job has
/// started (it is never returned). `exclude` skips one job (the one being
/// admitted).
pub(crate) fn consumable_in_use(st: &BatchState, name: &str, exclude: Option<&str>) -> i64 {
    let Some(res) = st.consumable_resources.get(name) else {
        return 0;
    };
    let arn = res
        .get("consumableResourceArn")
        .and_then(Value::as_str)
        .unwrap_or("");
    let replenishable =
        res.get("resourceType").and_then(Value::as_str) != Some("NON_REPLENISHABLE");
    st.jobs
        .iter()
        .filter(|(id, _)| Some(id.as_str()) != exclude)
        .filter(|(_, j)| {
            let status = j.get("status").and_then(Value::as_str).unwrap_or("");
            HOLDING.contains(&status)
                || (!replenishable
                    && matches!(status, "SUCCEEDED" | "FAILED")
                    && j.get("startedAt").is_some())
        })
        .flat_map(|(_, j)| job_consumable_requirements(j))
        .filter(|(r, _)| references(r, name, arn))
        .map(|(_, q)| q)
        .sum()
}

pub(crate) enum Admission {
    /// No requirements, or every requirement fits: the job may start.
    Admitted,
    /// At least one requirement does not fit right now: wait at RUNNABLE.
    Wait,
    /// The job was cancelled or terminated (or finished): never start it.
    Stopped,
}

/// Atomically admit a job against its consumable resources. On success with
/// requirements, the job is moved to STARTING under the same write lock so two
/// concurrently admitted jobs can never oversubscribe a resource. On a shortfall
/// the job is parked at RUNNABLE.
pub(crate) fn admit_job(st: &mut BatchState, job_id: &str) -> Admission {
    let Some(job) = st.jobs.get(job_id) else {
        return Admission::Admitted;
    };
    if matches!(
        job.get("status").and_then(Value::as_str),
        Some("SUCCEEDED" | "FAILED")
    ) {
        return Admission::Stopped;
    }
    let reqs = job_consumable_requirements(job);
    if reqs.is_empty() {
        return Admission::Admitted;
    }
    let fits = reqs.iter().all(|(reference, qty)| {
        let Some(key) = consumable_key(st, reference) else {
            return false;
        };
        let total = st
            .consumable_resources
            .get(&key)
            .and_then(|r| r.get("totalQuantity"))
            .and_then(Value::as_i64)
            .unwrap_or(0);
        consumable_in_use(st, &key, Some(job_id)) + qty <= total
    });
    let now = now_ms();
    if let Some(o) = st.jobs.get_mut(job_id).and_then(Value::as_object_mut) {
        if fits {
            o.insert("status".into(), json!("STARTING"));
        } else {
            if o.get("status").and_then(Value::as_str) != Some("RUNNABLE") {
                o.insert("runnableAt".into(), json!(now));
            }
            o.insert("status".into(), json!("RUNNABLE"));
        }
    }
    if fits {
        Admission::Admitted
    } else {
        Admission::Wait
    }
}

/// Validate that every resource a job requires exists.
pub(crate) fn validate_job_consumables(
    st: Option<&BatchState>,
    props: &Value,
) -> Result<(), AwsServiceError> {
    let probe = json!({ "consumableResourceProperties": props });
    for (reference, qty) in job_consumable_requirements(&probe) {
        if qty < 0 {
            return Err(client_error(
                "ClientException",
                "Consumable resource quantity must be non-negative",
            ));
        }
        if st.and_then(|s| consumable_key(s, &reference)).is_none() {
            return Err(client_error(
                "ClientException",
                format!("Consumable resource {reference} does not exist"),
            ));
        }
    }
    Ok(())
}

fn service_env_key(st: &BatchState, id: &str) -> Option<String> {
    let name = id.rsplit('/').next().unwrap_or(id);
    if st.service_environments.contains_key(name) {
        return Some(name.to_string());
    }
    st.service_environments
        .iter()
        .find(|(_, e)| e.get("serviceEnvironmentArn").and_then(Value::as_str) == Some(id))
        .map(|(k, _)| k.clone())
}

/// Validate a job queue's `serviceEnvironmentOrder` against
/// `computeEnvironmentOrder` and the existing service environments, returning
/// the service type those environments share (the queue's `jobQueueType`).
pub(crate) fn validate_service_environment_order(
    st: Option<&BatchState>,
    body: &Value,
) -> Result<Option<String>, AwsServiceError> {
    let Some(order) = body
        .get("serviceEnvironmentOrder")
        .and_then(Value::as_array)
        .filter(|a| !a.is_empty())
    else {
        return Ok(None);
    };
    if body
        .get("computeEnvironmentOrder")
        .and_then(Value::as_array)
        .is_some_and(|a| !a.is_empty())
    {
        return Err(client_error(
            "ClientException",
            "A job queue can't have both a serviceEnvironmentOrder and a computeEnvironmentOrder",
        ));
    }
    let mut kind: Option<String> = None;
    for entry in order {
        let id = entry
            .get("serviceEnvironment")
            .and_then(Value::as_str)
            .ok_or_else(|| client_error("ClientException", "serviceEnvironment is required"))?;
        let env = st
            .and_then(|s| service_env_key(s, id).and_then(|k| s.service_environments.get(&k)))
            .ok_or_else(|| {
                client_error(
                    "ClientException",
                    format!("Service environment {id} does not exist"),
                )
            })?;
        let ty = env
            .get("serviceEnvironmentType")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        match &kind {
            Some(k) if *k != ty => {
                return Err(client_error(
                    "ClientException",
                    "All service environments in serviceEnvironmentOrder must have the same type",
                ))
            }
            _ => kind = Some(ty),
        }
    }
    Ok(kind)
}

/// The scheduling policy attached to a job queue, if any.
fn queue_policy<'a>(st: &'a BatchState, queue: &Value) -> Option<&'a Value> {
    let arn = queue.get("schedulingPolicyArn").and_then(Value::as_str)?;
    st.scheduling_policies
        .values()
        .find(|p| p.get("arn").and_then(Value::as_str) == Some(arn))
}

/// Render a stored service job as `DescribeServiceJob` output.
fn service_job_detail(job: &Value, tags: &TagStore) -> Value {
    let mut out = project(
        job,
        &[
            "attempts",
            "capacityUsage",
            "createdAt",
            "isTerminated",
            "jobArn",
            "jobId",
            "jobName",
            "jobQueue",
            "retryStrategy",
            "scheduledAt",
            "schedulingPriority",
            "serviceRequestPayload",
            "serviceJobType",
            "shareIdentifier",
            "quotaShareName",
            "preemptionConfiguration",
            "preemptionSummary",
            "startedAt",
            "status",
            "statusReason",
            "stoppedAt",
            "timeoutConfig",
        ],
    );
    if let Some(last) = job
        .get("attempts")
        .and_then(Value::as_array)
        .and_then(|a| a.last())
    {
        out.insert(
            "latestAttempt".into(),
            json!(project(last, &["serviceResourceId"])),
        );
    }
    if let Some(arn) = job.get("jobArn").and_then(Value::as_str) {
        if let Some(t) = tags_value(tags, arn) {
            out.insert("tags".into(), t);
        }
    }
    Value::Object(out)
}

fn service_job_summary(job: &Value) -> Value {
    let mut out = project(
        job,
        &[
            "capacityUsage",
            "createdAt",
            "jobArn",
            "jobId",
            "jobName",
            "scheduledAt",
            "serviceJobType",
            "shareIdentifier",
            "quotaShareName",
            "status",
            "statusReason",
            "startedAt",
            "stoppedAt",
        ],
    );
    if let Some(last) = job
        .get("attempts")
        .and_then(Value::as_array)
        .and_then(|a| a.last())
    {
        out.insert(
            "latestAttempt".into(),
            json!(project(last, &["serviceResourceId"])),
        );
    }
    Value::Object(out)
}

fn quota_share_detail(share: &Value) -> Map<String, Value> {
    project(
        share,
        &[
            "quotaShareName",
            "quotaShareArn",
            "jobQueueArn",
            "capacityLimits",
            "resourceSharingConfiguration",
            "preemptionConfiguration",
            "state",
            "status",
        ],
    )
}

fn validate_quota_share_config(body: &Value) -> Result<(), AwsServiceError> {
    if let Some(limits) = body.get("capacityLimits") {
        let arr = limits
            .as_array()
            .ok_or_else(|| client_error("ClientException", "capacityLimits must be a list"))?;
        for l in arr {
            let max = l.get("maxCapacity").and_then(Value::as_i64);
            let unit = l.get("capacityUnit").and_then(Value::as_str);
            match (max, unit) {
                (Some(m), Some(u)) if m >= 0 && !u.is_empty() => {}
                _ => {
                    return Err(client_error(
                        "ClientException",
                        "Each capacityLimit requires a non-negative maxCapacity and a capacityUnit",
                    ))
                }
            }
        }
    }
    if let Some(rsc) = body.get("resourceSharingConfiguration") {
        let strategy = rsc.get("strategy");
        if strategy.is_none() {
            return Err(client_error(
                "ClientException",
                "resourceSharingConfiguration.strategy is required",
            ));
        }
        validate_enum(
            "resourceSharingConfiguration.strategy",
            strategy,
            &["RESERVE", "LEND", "LEND_AND_BORROW"],
        )?;
        if let Some(b) = rsc.get("borrowLimit").and_then(Value::as_i64) {
            if strategy.and_then(Value::as_str) != Some("LEND_AND_BORROW") {
                return Err(client_error(
                    "ClientException",
                    "borrowLimit can only be specified with the LEND_AND_BORROW strategy",
                ));
            }
            if b < -1 {
                return Err(client_error(
                    "ClientException",
                    "borrowLimit must be -1 (unlimited) or a non-negative percentage",
                ));
            }
        }
    }
    if let Some(pc) = body.get("preemptionConfiguration") {
        let v = pc.get("inSharePreemption");
        if v.is_none() {
            return Err(client_error(
                "ClientException",
                "preemptionConfiguration.inSharePreemption is required",
            ));
        }
        validate_enum(
            "preemptionConfiguration.inSharePreemption",
            v,
            &["ENABLED", "DISABLED"],
        )?;
    }
    validate_enum("state", body.get("state"), &["ENABLED", "DISABLED"])
}

fn validate_capacity_limits(body: &Value, required: bool) -> Result<(), AwsServiceError> {
    match body.get("capacityLimits") {
        None if required => Err(client_error(
            "ClientException",
            "capacityLimits is required",
        )),
        None => Ok(()),
        Some(v) => {
            let arr = v
                .as_array()
                .ok_or_else(|| client_error("ClientException", "capacityLimits must be a list"))?;
            if arr.is_empty() {
                return Err(client_error(
                    "ClientException",
                    "capacityLimits must contain at least one capacity limit",
                ));
            }
            for l in arr {
                if l.get("maxCapacity")
                    .and_then(Value::as_i64)
                    .is_some_and(|m| m < 0)
                {
                    return Err(client_error(
                        "ClientException",
                        "maxCapacity must be non-negative",
                    ));
                }
            }
            Ok(())
        }
    }
}

impl BatchService {
    // ---- Consumable resources ----

    pub(crate) fn create_consumable_resource(
        &self,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        let body = req.json_body();
        let name = required_str(&body, "consumableResourceName")?.to_string();
        validate_name("consumableResourceName", &name)?;
        let total = body
            .get("totalQuantity")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        if total < 0 {
            return Err(client_error(
                "ClientException",
                "totalQuantity must be non-negative",
            ));
        }
        validate_enum(
            "resourceType",
            body.get("resourceType"),
            &["REPLENISHABLE", "NON_REPLENISHABLE"],
        )?;
        let resource_type = body
            .get("resourceType")
            .and_then(Value::as_str)
            .unwrap_or("REPLENISHABLE");
        let arn = batch_arn(
            &req.region,
            &req.account_id,
            &format!("consumable-resource/{name}"),
        );
        let mut accounts = self.state.write();
        let st = accounts.get_or_create(&req.account_id);
        if st.consumable_resources.contains_key(&name) {
            return Err(client_error(
                "ClientException",
                format!("Object already exists: {name}"),
            ));
        }
        let stored = json!({
            "consumableResourceName": name,
            "consumableResourceArn": arn,
            "totalQuantity": total,
            "resourceType": resource_type,
            "createdAt": now_ms(),
        });
        seed_inline_tags(&mut st.tags, &arn, &obj(&body));
        st.consumable_resources.insert(name.clone(), stored);
        Ok(AwsResponse::ok_json(json!({
            "consumableResourceName": name,
            "consumableResourceArn": arn,
        })))
    }

    pub(crate) fn describe_consumable_resource(
        &self,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        let body = req.json_body();
        let id = required_str(&body, "consumableResource")?;
        let accounts = self.state.read();
        let st = accounts.get(&req.account_id);
        let (key, res) = st
            .and_then(|s| {
                consumable_key(s, id).and_then(|k| s.consumable_resources.get(&k).map(|r| (k, r)))
            })
            .ok_or_else(|| {
                client_error(
                    "ClientException",
                    format!("Consumable resource {id} does not exist"),
                )
            })?;
        let st = st.expect("resource found implies account state");
        let total = res
            .get("totalQuantity")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let in_use = consumable_in_use(st, &key, None);
        let mut out = project(
            res,
            &[
                "consumableResourceName",
                "consumableResourceArn",
                "totalQuantity",
                "resourceType",
                "createdAt",
            ],
        );
        out.insert("inUseQuantity".into(), json!(in_use));
        out.insert("availableQuantity".into(), json!((total - in_use).max(0)));
        let arn = res
            .get("consumableResourceArn")
            .and_then(Value::as_str)
            .unwrap_or("");
        if let Some(t) = tags_value(&st.tags, arn) {
            out.insert("tags".into(), t);
        }
        Ok(AwsResponse::ok_json(Value::Object(out)))
    }

    pub(crate) fn list_consumable_resources(
        &self,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        let body = req.json_body();
        let filters = parse_filters(&body);
        for (name, _) in &filters {
            if name != "CONSUMABLE_RESOURCE_NAME" {
                return Err(client_error(
                    "ClientException",
                    format!("Unsupported filter name: {name}"),
                ));
            }
        }
        let accounts = self.state.read();
        let items: Vec<Value> = accounts
            .get(&req.account_id)
            .map(|st| {
                st.consumable_resources
                    .iter()
                    .filter(|(name, _)| {
                        filters
                            .iter()
                            .all(|(_, values)| values.iter().any(|v| name_filter_matches(v, name)))
                    })
                    .map(|(name, r)| {
                        let mut s = project(
                            r,
                            &[
                                "consumableResourceArn",
                                "consumableResourceName",
                                "totalQuantity",
                                "resourceType",
                            ],
                        );
                        s.insert(
                            "inUseQuantity".into(),
                            json!(consumable_in_use(st, name, None)),
                        );
                        Value::Object(s)
                    })
                    .collect()
            })
            .unwrap_or_default();
        let (page, next) = paginate(items, &body)?;
        let mut resp = Map::new();
        resp.insert("consumableResources".into(), Value::Array(page));
        if let Some(n) = next {
            resp.insert("nextToken".into(), json!(n));
        }
        Ok(AwsResponse::ok_json(Value::Object(resp)))
    }

    pub(crate) fn update_consumable_resource(
        &self,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        let body = req.json_body();
        let id = required_str(&body, "consumableResource")?.to_string();
        validate_enum(
            "operation",
            body.get("operation"),
            &["SET", "ADD", "REMOVE"],
        )?;
        let operation = body
            .get("operation")
            .and_then(Value::as_str)
            .unwrap_or("SET");
        let quantity = body.get("quantity").and_then(Value::as_i64);
        if quantity.is_some_and(|q| q < 0) {
            return Err(client_error(
                "ClientException",
                "quantity must be non-negative",
            ));
        }
        let token = body
            .get("clientToken")
            .and_then(Value::as_str)
            .map(String::from);
        let now = now_ms();
        let mut accounts = self.state.write();
        let st = accounts.get_or_create(&req.account_id);
        // Idempotency: an identical request replayed with the same clientToken
        // succeeds but is applied only once.
        st.consumable_update_tokens.retain(|_, rec| {
            rec.get("at").and_then(Value::as_i64).unwrap_or(0) + CLIENT_TOKEN_TTL_MS > now
        });
        if let Some(t) = &token {
            if let Some(rec) = st.consumable_update_tokens.get(t) {
                if rec.get("request") == Some(&body) {
                    return Ok(AwsResponse::ok_json(
                        rec.get("response").cloned().unwrap_or_else(|| json!({})),
                    ));
                }
                return Err(client_error(
                    "ClientException",
                    "The clientToken was already used for a different request",
                ));
            }
        }
        let key = consumable_key(st, &id).ok_or_else(|| {
            client_error(
                "ClientException",
                format!("Consumable resource {id} does not exist"),
            )
        })?;
        let res = st
            .consumable_resources
            .get_mut(&key)
            .and_then(Value::as_object_mut)
            .expect("key resolved from the store");
        let current = res
            .get("totalQuantity")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let new_total = match (operation, quantity) {
            (_, None) => current,
            ("ADD", Some(q)) => current.saturating_add(q),
            ("REMOVE", Some(q)) => {
                if q > current {
                    return Err(client_error(
                        "ClientException",
                        format!(
                            "Cannot remove {q} from consumable resource {key}: only {current} available"
                        ),
                    ));
                }
                current - q
            }
            (_, Some(q)) => q,
        };
        res.insert("totalQuantity".into(), json!(new_total));
        let response = json!({
            "consumableResourceName": key,
            "consumableResourceArn": res.get("consumableResourceArn").cloned().unwrap_or(Value::Null),
            "totalQuantity": new_total,
        });
        if let Some(t) = token {
            st.consumable_update_tokens.insert(
                t,
                json!({ "request": body, "response": response.clone(), "at": now }),
            );
        }
        Ok(AwsResponse::ok_json(response))
    }

    pub(crate) fn delete_consumable_resource(
        &self,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        let body = req.json_body();
        let id = required_str(&body, "consumableResource")?.to_string();
        let mut accounts = self.state.write();
        let st = accounts.get_or_create(&req.account_id);
        let key = consumable_key(st, &id).ok_or_else(|| {
            client_error(
                "ClientException",
                format!("Consumable resource {id} does not exist"),
            )
        })?;
        if let Some(res) = st.consumable_resources.remove(&key) {
            if let Some(arn) = res.get("consumableResourceArn").and_then(Value::as_str) {
                st.tags.remove(arn);
            }
        }
        Ok(AwsResponse::ok_json(json!({})))
    }

    pub(crate) fn list_jobs_by_consumable_resource(
        &self,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        let body = req.json_body();
        let id = required_str(&body, "consumableResource")?;
        let filters = parse_filters(&body);
        for (name, _) in &filters {
            if name != "JOB_STATUS" && name != "JOB_NAME" {
                return Err(client_error(
                    "ClientException",
                    format!("Unsupported filter name: {name}"),
                ));
            }
        }
        let accounts = self.state.read();
        let st = accounts.get(&req.account_id);
        let key = st.and_then(|s| consumable_key(s, id)).ok_or_else(|| {
            client_error(
                "ClientException",
                format!("Consumable resource {id} does not exist"),
            )
        })?;
        let st = st.expect("resource found implies account state");
        let arn = st
            .consumable_resources
            .get(&key)
            .and_then(|r| r.get("consumableResourceArn"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let mut matched: Vec<&Value> = st
            .jobs
            .values()
            .filter(|j| {
                job_consumable_requirements(j)
                    .iter()
                    .any(|(r, _)| references(r, &key, arn))
            })
            .filter(|j| {
                let status = j.get("status").and_then(Value::as_str).unwrap_or("");
                let name = j.get("jobName").and_then(Value::as_str).unwrap_or("");
                filters.iter().all(|(f, values)| match f.as_str() {
                    "JOB_STATUS" => values.iter().any(|v| v == status),
                    _ => values.iter().any(|v| name_filter_matches(v, name)),
                })
            })
            .collect();
        matched.sort_by_key(|j| {
            std::cmp::Reverse(j.get("createdAt").and_then(Value::as_i64).unwrap_or(0))
        });
        let items: Vec<Value> = matched
            .into_iter()
            .map(|j| {
                let quantity: i64 = job_consumable_requirements(j)
                    .iter()
                    .filter(|(r, _)| references(r, &key, arn))
                    .map(|(_, q)| q)
                    .sum();
                let queue = j.get("jobQueue").and_then(Value::as_str).unwrap_or("");
                let queue_arn = find_queue(st, queue)
                    .and_then(|q| q.get("jobQueueArn"))
                    .and_then(Value::as_str)
                    .unwrap_or(queue);
                let mut s = Map::new();
                s.insert(
                    "jobArn".into(),
                    j.get("jobArn").cloned().unwrap_or(Value::Null),
                );
                s.insert("jobQueueArn".into(), json!(queue_arn));
                s.insert(
                    "jobName".into(),
                    j.get("jobName").cloned().unwrap_or(Value::Null),
                );
                if let Some(jd) = self.job_definition_arn(st, j) {
                    s.insert("jobDefinitionArn".into(), json!(jd));
                }
                for k in ["shareIdentifier", "statusReason", "startedAt"] {
                    if let Some(v) = j.get(k) {
                        s.insert(k.into(), v.clone());
                    }
                }
                s.insert(
                    "jobStatus".into(),
                    j.get("status").cloned().unwrap_or(Value::Null),
                );
                s.insert("quantity".into(), json!(quantity));
                s.insert(
                    "createdAt".into(),
                    j.get("createdAt").cloned().unwrap_or(Value::Null),
                );
                s.insert(
                    "consumableResourceProperties".into(),
                    j.get("consumableResourceProperties")
                        .cloned()
                        .unwrap_or_else(|| json!({})),
                );
                Value::Object(s)
            })
            .collect();
        let (page, next) = paginate(items, &body)?;
        let mut resp = Map::new();
        resp.insert("jobs".into(), Value::Array(page));
        if let Some(n) = next {
            resp.insert("nextToken".into(), json!(n));
        }
        Ok(AwsResponse::ok_json(Value::Object(resp)))
    }

    /// Resolve a job's `jobDefinition` reference (name, name:revision or ARN)
    /// to the job definition ARN.
    fn job_definition_arn(&self, st: &BatchState, job: &Value) -> Option<String> {
        let reference = job.get("jobDefinition").and_then(Value::as_str)?;
        if reference.starts_with("arn:") {
            return Some(reference.to_string());
        }
        let jd = if reference.contains(':') {
            st.job_definitions.get(reference)
        } else {
            st.job_definitions
                .iter()
                .filter(|(k, _)| k.rsplit_once(':').map(|(n, _)| n) == Some(reference))
                .max_by_key(|(k, _)| {
                    k.rsplit_once(':')
                        .and_then(|(_, r)| r.parse::<i64>().ok())
                        .unwrap_or(0)
                })
                .map(|(_, v)| v)
        };
        jd.and_then(|d| d.get("jobDefinitionArn"))
            .and_then(Value::as_str)
            .map(String::from)
    }

    // ---- Service environments ----

    pub(crate) fn create_service_environment(
        &self,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        let body = req.json_body();
        let name = required_str(&body, "serviceEnvironmentName")?.to_string();
        validate_name("serviceEnvironmentName", &name)?;
        if body.get("serviceEnvironmentType").is_none() {
            return Err(client_error(
                "ClientException",
                "serviceEnvironmentType is required",
            ));
        }
        validate_enum(
            "serviceEnvironmentType",
            body.get("serviceEnvironmentType"),
            &["SAGEMAKER_TRAINING"],
        )?;
        validate_enum("state", body.get("state"), &["ENABLED", "DISABLED"])?;
        validate_capacity_limits(&body, true)?;
        let arn = batch_arn(
            &req.region,
            &req.account_id,
            &format!("service-environment/{name}"),
        );
        let mut accounts = self.state.write();
        let st = accounts.get_or_create(&req.account_id);
        if st.service_environments.contains_key(&name) {
            return Err(client_error(
                "ClientException",
                format!("Object already exists: {name}"),
            ));
        }
        let stored = json!({
            "serviceEnvironmentName": name,
            "serviceEnvironmentArn": arn,
            "serviceEnvironmentType": body["serviceEnvironmentType"],
            "state": body.get("state").cloned().unwrap_or_else(|| json!("ENABLED")),
            "status": "VALID",
            "capacityLimits": body["capacityLimits"],
        });
        seed_inline_tags(&mut st.tags, &arn, &obj(&body));
        st.service_environments.insert(name.clone(), stored);
        Ok(AwsResponse::ok_json(json!({
            "serviceEnvironmentName": name,
            "serviceEnvironmentArn": arn,
        })))
    }

    pub(crate) fn describe_service_environments(
        &self,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        let body = req.json_body();
        let wanted = string_set(&body, "serviceEnvironments");
        let accounts = self.state.read();
        let items: Vec<Value> = accounts
            .get(&req.account_id)
            .map(|st| {
                st.service_environments
                    .values()
                    .filter(|e| {
                        wanted.is_empty()
                            || ["serviceEnvironmentName", "serviceEnvironmentArn"]
                                .iter()
                                .filter_map(|k| e.get(*k).and_then(Value::as_str))
                                .any(|v| wanted.contains(v))
                    })
                    .map(|e| {
                        let mut o = project(
                            e,
                            &[
                                "serviceEnvironmentName",
                                "serviceEnvironmentArn",
                                "serviceEnvironmentType",
                                "state",
                                "status",
                                "capacityLimits",
                            ],
                        );
                        let arn = e
                            .get("serviceEnvironmentArn")
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        if let Some(t) = tags_value(&st.tags, arn) {
                            o.insert("tags".into(), t);
                        }
                        Value::Object(o)
                    })
                    .collect()
            })
            .unwrap_or_default();
        let (page, next) = paginate(items, &body)?;
        let mut resp = Map::new();
        resp.insert("serviceEnvironments".into(), Value::Array(page));
        if let Some(n) = next {
            resp.insert("nextToken".into(), json!(n));
        }
        Ok(AwsResponse::ok_json(Value::Object(resp)))
    }

    pub(crate) fn update_service_environment(
        &self,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        let body = req.json_body();
        let id = required_str(&body, "serviceEnvironment")?.to_string();
        validate_enum("state", body.get("state"), &["ENABLED", "DISABLED"])?;
        validate_capacity_limits(&body, false)?;
        let mut accounts = self.state.write();
        let st = accounts.get_or_create(&req.account_id);
        let key = service_env_key(st, &id).ok_or_else(|| {
            client_error(
                "ClientException",
                format!("Service environment {id} does not exist"),
            )
        })?;
        let env = st
            .service_environments
            .get_mut(&key)
            .and_then(Value::as_object_mut)
            .expect("key resolved from the store");
        for f in ["state", "capacityLimits"] {
            if let Some(v) = body.get(f) {
                env.insert(f.into(), v.clone());
            }
        }
        Ok(AwsResponse::ok_json(json!({
            "serviceEnvironmentName": key,
            "serviceEnvironmentArn": env.get("serviceEnvironmentArn").cloned().unwrap_or(Value::Null),
        })))
    }

    pub(crate) fn delete_service_environment(
        &self,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        let body = req.json_body();
        let id = required_str(&body, "serviceEnvironment")?.to_string();
        let mut accounts = self.state.write();
        let st = accounts.get_or_create(&req.account_id);
        let key = service_env_key(st, &id).ok_or_else(|| {
            client_error(
                "ClientException",
                format!("Service environment {id} does not exist"),
            )
        })?;
        let env = &st.service_environments[&key];
        if env.get("state").and_then(Value::as_str) != Some("DISABLED") {
            return Err(client_error(
                "ClientException",
                format!("Service environment {key} must be DISABLED before it can be deleted"),
            ));
        }
        let arn = env
            .get("serviceEnvironmentArn")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let attached = st.job_queues.values().find(|q| {
            q.get("serviceEnvironmentOrder")
                .and_then(Value::as_array)
                .is_some_and(|order| {
                    order.iter().any(|o| {
                        o.get("serviceEnvironment")
                            .and_then(Value::as_str)
                            .is_some_and(|r| references(r, &key, &arn))
                    })
                })
        });
        if let Some(q) = attached {
            let qn = q.get("jobQueueName").and_then(Value::as_str).unwrap_or("");
            return Err(client_error(
                "ClientException",
                format!("Service environment {key} is still associated with job queue {qn}"),
            ));
        }
        st.service_environments.remove(&key);
        st.tags.remove(&arn);
        Ok(AwsResponse::ok_json(json!({})))
    }

    // ---- Quota shares ----

    pub(crate) fn create_quota_share(
        &self,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        let body = req.json_body();
        let name = required_str(&body, "quotaShareName")?.to_string();
        validate_name("quotaShareName", &name)?;
        let queue_id = required_str(&body, "jobQueue")?.to_string();
        for f in [
            "capacityLimits",
            "resourceSharingConfiguration",
            "preemptionConfiguration",
        ] {
            if body.get(f).is_none() {
                return Err(client_error("ClientException", format!("{f} is required")));
            }
        }
        validate_quota_share_config(&body)?;
        let mut accounts = self.state.write();
        let st = accounts.get_or_create(&req.account_id);
        let queue = find_queue(st, &queue_id).ok_or_else(|| {
            client_error(
                "ClientException",
                format!("Job queue {queue_id} does not exist"),
            )
        })?;
        let queue_name = queue
            .get("jobQueueName")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let queue_arn = queue
            .get("jobQueueArn")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let arn = batch_arn(
            &req.region,
            &req.account_id,
            &format!("job-queue/{queue_name}/quota-share/{name}"),
        );
        if st.quota_shares.contains_key(&arn) {
            return Err(client_error(
                "ClientException",
                format!("Object already exists: {name}"),
            ));
        }
        let stored = json!({
            "quotaShareName": name,
            "quotaShareArn": arn,
            "jobQueueArn": queue_arn,
            "capacityLimits": body["capacityLimits"],
            "resourceSharingConfiguration": body["resourceSharingConfiguration"],
            "preemptionConfiguration": body["preemptionConfiguration"],
            "state": body.get("state").cloned().unwrap_or_else(|| json!("ENABLED")),
            "status": "VALID",
        });
        seed_inline_tags(&mut st.tags, &arn, &obj(&body));
        st.quota_shares.insert(arn.clone(), stored);
        Ok(AwsResponse::ok_json(json!({
            "quotaShareName": name,
            "quotaShareArn": arn,
        })))
    }

    pub(crate) fn describe_quota_share(
        &self,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        let body = req.json_body();
        let arn = required_str(&body, "quotaShareArn")?;
        let accounts = self.state.read();
        let st = accounts.get(&req.account_id);
        let share = st.and_then(|s| s.quota_shares.get(arn)).ok_or_else(|| {
            client_error(
                "ClientException",
                format!("Quota share {arn} does not exist"),
            )
        })?;
        let mut out = quota_share_detail(share);
        if let Some(t) = st.and_then(|s| tags_value(&s.tags, arn)) {
            out.insert("tags".into(), t);
        }
        Ok(AwsResponse::ok_json(Value::Object(out)))
    }

    pub(crate) fn list_quota_shares(
        &self,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        let body = req.json_body();
        let queue_id = required_str(&body, "jobQueue")?;
        let accounts = self.state.read();
        let st = accounts.get(&req.account_id);
        let queue_arn = st
            .and_then(|s| find_queue(s, queue_id))
            .and_then(|q| q.get("jobQueueArn"))
            .and_then(Value::as_str)
            .ok_or_else(|| {
                client_error(
                    "ClientException",
                    format!("Job queue {queue_id} does not exist"),
                )
            })?;
        let items: Vec<Value> = st
            .map(|s| {
                s.quota_shares
                    .values()
                    .filter(|q| q.get("jobQueueArn").and_then(Value::as_str) == Some(queue_arn))
                    .map(|q| Value::Object(quota_share_detail(q)))
                    .collect()
            })
            .unwrap_or_default();
        let (page, next) = paginate(items, &body)?;
        let mut resp = Map::new();
        resp.insert("quotaShares".into(), Value::Array(page));
        if let Some(n) = next {
            resp.insert("nextToken".into(), json!(n));
        }
        Ok(AwsResponse::ok_json(Value::Object(resp)))
    }

    pub(crate) fn update_quota_share(
        &self,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        let body = req.json_body();
        let arn = required_str(&body, "quotaShareArn")?.to_string();
        validate_quota_share_config(&body)?;
        let mut accounts = self.state.write();
        let st = accounts.get_or_create(&req.account_id);
        let share = st
            .quota_shares
            .get_mut(&arn)
            .and_then(Value::as_object_mut)
            .ok_or_else(|| {
                client_error(
                    "ClientException",
                    format!("Quota share {arn} does not exist"),
                )
            })?;
        for f in [
            "capacityLimits",
            "resourceSharingConfiguration",
            "preemptionConfiguration",
            "state",
        ] {
            if let Some(v) = body.get(f) {
                share.insert(f.into(), v.clone());
            }
        }
        Ok(AwsResponse::ok_json(json!({
            "quotaShareName": share.get("quotaShareName").cloned().unwrap_or(Value::Null),
            "quotaShareArn": arn,
        })))
    }

    pub(crate) fn delete_quota_share(
        &self,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        let body = req.json_body();
        let arn = required_str(&body, "quotaShareArn")?.to_string();
        let mut accounts = self.state.write();
        let st = accounts.get_or_create(&req.account_id);
        let share = st.quota_shares.get(&arn).ok_or_else(|| {
            client_error(
                "ClientException",
                format!("Quota share {arn} does not exist"),
            )
        })?;
        if share.get("state").and_then(Value::as_str) != Some("DISABLED") {
            return Err(client_error(
                "ClientException",
                "The quota share must be DISABLED before it can be deleted",
            ));
        }
        let name = share
            .get("quotaShareName")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let queue_arn = share
            .get("jobQueueArn")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        // Deleting a quota share terminates the jobs still in it.
        let now = now_ms();
        for job in st.service_jobs.values_mut() {
            let Some(o) = job.as_object_mut() else {
                continue;
            };
            let in_share = o.get("quotaShareName").and_then(Value::as_str) == Some(name.as_str())
                && o.get("jobQueue").and_then(Value::as_str) == Some(queue_arn.as_str());
            let status = o.get("status").and_then(Value::as_str).unwrap_or("");
            if in_share && SERVICE_JOB_ACTIVE.contains(&status) {
                o.insert("status".into(), json!("FAILED"));
                o.insert("statusReason".into(), json!("Quota share deleted"));
                o.insert("isTerminated".into(), json!(true));
                o.insert("stoppedAt".into(), json!(now));
            }
        }
        st.quota_shares.remove(&arn);
        st.tags.remove(&arn);
        Ok(AwsResponse::ok_json(json!({})))
    }

    // ---- Service jobs ----

    pub(crate) fn submit_service_job(
        &self,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        let body = req.json_body();
        let job_name = required_str(&body, "jobName")?.to_string();
        validate_name("jobName", &job_name)?;
        let queue_id = required_str(&body, "jobQueue")?.to_string();
        let payload = required_str(&body, "serviceRequestPayload")?.to_string();
        if body.get("serviceJobType").is_none() {
            return Err(client_error(
                "ClientException",
                "serviceJobType is required",
            ));
        }
        validate_enum(
            "serviceJobType",
            body.get("serviceJobType"),
            &["SAGEMAKER_TRAINING"],
        )?;
        let job_type = body["serviceJobType"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let parsed: Value = serde_json::from_str(&payload).map_err(|_| {
            client_error(
                "ClientException",
                "serviceRequestPayload must be valid JSON",
            )
        })?;
        if !parsed.is_object() {
            return Err(client_error(
                "ClientException",
                "serviceRequestPayload must be a JSON object",
            ));
        }
        if let Some(p) = body.get("schedulingPriority").and_then(Value::as_i64) {
            if !(0..=9999).contains(&p) {
                return Err(client_error(
                    "ClientException",
                    "schedulingPriority must be between 0 and 9999",
                ));
            }
        }
        if let Some(rs) = body.get("retryStrategy") {
            match rs.get("attempts").and_then(Value::as_i64) {
                Some(a) if (1..=10).contains(&a) => {}
                _ => {
                    return Err(client_error(
                        "ClientException",
                        "retryStrategy.attempts must be between 1 and 10",
                    ))
                }
            }
        }
        let share_identifier = body.get("shareIdentifier").and_then(Value::as_str);
        let quota_share_name = body.get("quotaShareName").and_then(Value::as_str);
        let token = body.get("clientToken").and_then(Value::as_str);

        let mut accounts = self.state.write();
        let st = accounts.get_or_create(&req.account_id);
        let queue = find_queue(st, &queue_id).ok_or_else(|| {
            client_error(
                "ClientException",
                format!("Job queue {queue_id} does not exist"),
            )
        })?;
        if queue.get("jobQueueType").and_then(Value::as_str) != Some(job_type.as_str()) {
            return Err(client_error(
                "ClientException",
                format!("Job queue {queue_id} does not have the type {job_type}"),
            ));
        }
        if queue.get("state").and_then(Value::as_str) == Some("DISABLED") {
            return Err(client_error(
                "ClientException",
                format!("Job queue {queue_id} is DISABLED and can't accept new jobs"),
            ));
        }
        let queue_arn = queue
            .get("jobQueueArn")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let policy = queue_policy(st, queue);
        let fair_share = policy.is_some_and(|p| p.get("fairsharePolicy").is_some());
        let quota_policy = policy.is_some_and(|p| p.get("quotaSharePolicy").is_some());
        match (fair_share, share_identifier) {
            (true, None) => {
                return Err(client_error(
                    "ClientException",
                    "shareIdentifier is required for a job queue with a fair-share scheduling policy",
                ))
            }
            (false, Some(_)) => {
                return Err(client_error(
                    "ClientException",
                    "shareIdentifier can't be specified for a job queue without a fair-share scheduling policy",
                ))
            }
            _ => {}
        }
        match (quota_policy, quota_share_name) {
            (true, None) => {
                return Err(client_error(
                    "ClientException",
                    "quotaShareName is required for a job queue with a quota share scheduling policy",
                ))
            }
            (false, Some(_)) => {
                return Err(client_error(
                    "ClientException",
                    "quotaShareName can't be specified for a job queue without a quota share scheduling policy",
                ))
            }
            (true, Some(qs)) => {
                let share = st.quota_shares.values().find(|s| {
                    s.get("jobQueueArn").and_then(Value::as_str) == Some(queue_arn.as_str())
                        && s.get("quotaShareName").and_then(Value::as_str) == Some(qs)
                });
                match share {
                    None => {
                        return Err(client_error(
                            "ClientException",
                            format!("Quota share {qs} does not exist in job queue {queue_id}"),
                        ))
                    }
                    Some(s) if s.get("state").and_then(Value::as_str) == Some("DISABLED") => {
                        return Err(client_error(
                            "ClientException",
                            format!("Quota share {qs} is DISABLED and can't accept new jobs"),
                        ))
                    }
                    Some(_) => {}
                }
            }
            (false, None) => {}
        }
        if let Some(t) = token {
            if st
                .service_jobs
                .values()
                .any(|j| j.get("clientToken").and_then(Value::as_str) == Some(t))
            {
                return Err(client_error(
                    "ClientException",
                    "A service job was already submitted with this clientToken",
                ));
            }
        }

        // Capacity is metered in the queue's service environment unit:
        // NUM_INSTANCES, or the instance type for a quota-managed environment.
        let instance_count = parsed
            .pointer("/ResourceConfig/InstanceCount")
            .and_then(Value::as_f64)
            .unwrap_or(1.0);
        let instance_type = parsed
            .pointer("/ResourceConfig/InstanceType")
            .and_then(Value::as_str);
        let env_unit = queue
            .get("serviceEnvironmentOrder")
            .and_then(Value::as_array)
            .and_then(|o| o.first())
            .and_then(|o| o.get("serviceEnvironment"))
            .and_then(Value::as_str)
            .and_then(|id| service_env_key(st, id))
            .and_then(|k| st.service_environments.get(&k))
            .and_then(|e| e.pointer("/capacityLimits/0/capacityUnit"))
            .and_then(Value::as_str);
        let capacity_unit = match (env_unit, instance_type) {
            (Some(u), Some(it)) if u != "NUM_INSTANCES" => it.to_string(),
            _ => "NUM_INSTANCES".to_string(),
        };

        let job_id = Uuid::new_v4().to_string();
        let arn = batch_arn(
            &req.region,
            &req.account_id,
            &format!("service-job/{job_id}"),
        );
        let now = now_ms();
        let mut job = project(
            &body,
            &[
                "retryStrategy",
                "schedulingPriority",
                "shareIdentifier",
                "quotaShareName",
                "preemptionConfiguration",
                "timeoutConfig",
                "clientToken",
            ],
        );
        job.insert("jobId".into(), json!(job_id));
        job.insert("jobArn".into(), json!(arn));
        job.insert("jobName".into(), json!(job_name));
        job.insert("jobQueue".into(), json!(queue_arn));
        job.insert("serviceJobType".into(), json!(job_type));
        job.insert("serviceRequestPayload".into(), json!(payload));
        job.insert("createdAt".into(), json!(now));
        job.insert(
            "capacityUsage".into(),
            json!([{ "capacityUnit": capacity_unit, "quantity": instance_count }]),
        );
        job.insert("attempts".into(), json!([]));
        job.insert("isTerminated".into(), json!(false));
        // The scheduler accepts the job into the queue (SUBMITTED -> RUNNABLE).
        // fakecloud has no SageMaker training executor to dispatch it to, so it
        // waits at RUNNABLE rather than reporting a fabricated training run.
        job.insert("status".into(), json!("RUNNABLE"));
        job.insert("runnableAt".into(), json!(now));
        seed_inline_tags(&mut st.tags, &arn, &obj(&body));
        st.service_jobs.insert(job_id.clone(), Value::Object(job));
        Ok(AwsResponse::ok_json(json!({
            "jobArn": arn,
            "jobName": job_name,
            "jobId": job_id,
        })))
    }

    pub(crate) fn describe_service_job(
        &self,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        let body = req.json_body();
        let id = required_str(&body, "jobId")?;
        let accounts = self.state.read();
        let st = accounts.get(&req.account_id);
        let job = st.and_then(|s| s.service_jobs.get(id)).ok_or_else(|| {
            client_error(
                "ClientException",
                format!("Service job {id} does not exist"),
            )
        })?;
        let tags = st
            .map(|s| &s.tags)
            .expect("job found implies account state");
        Ok(AwsResponse::ok_json(service_job_detail(job, tags)))
    }

    pub(crate) fn list_service_jobs(
        &self,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        let body = req.json_body();
        let filters = parse_filters(&body);
        if filters.len() > 1 {
            return Err(client_error(
                "ClientException",
                "Only one filter can be used at a time",
            ));
        }
        let filter = filters.into_iter().next();
        if let Some((name, values)) = &filter {
            if ![
                "JOB_NAME",
                "BEFORE_CREATED_AT",
                "AFTER_CREATED_AT",
                "SHARE_IDENTIFIER",
                "QUOTA_SHARE_NAME",
            ]
            .contains(&name.as_str())
            {
                return Err(client_error(
                    "ClientException",
                    format!("Unsupported filter name: {name}"),
                ));
            }
            if values.is_empty() {
                return Err(client_error(
                    "ClientException",
                    format!("Filter {name} requires a value"),
                ));
            }
        }
        validate_enum(
            "jobStatus",
            body.get("jobStatus"),
            &[
                "SUBMITTED",
                "PENDING",
                "RUNNABLE",
                "SCHEDULED",
                "STARTING",
                "RUNNING",
                "SUCCEEDED",
                "FAILED",
            ],
        )?;
        // jobStatus (default RUNNING) applies unless a filter other than
        // SHARE_IDENTIFIER / QUOTA_SHARE_NAME is used.
        let status_applies = filter
            .as_ref()
            .is_none_or(|(n, _)| n == "SHARE_IDENTIFIER" || n == "QUOTA_SHARE_NAME");
        let status = body
            .get("jobStatus")
            .and_then(Value::as_str)
            .unwrap_or("RUNNING")
            .to_string();
        let accounts = self.state.read();
        let st = accounts.get(&req.account_id);
        let queue_arn = match body.get("jobQueue").and_then(Value::as_str) {
            None => None,
            Some(q) => Some(
                st.and_then(|s| find_queue(s, q))
                    .and_then(|v| v.get("jobQueueArn"))
                    .and_then(Value::as_str)
                    .map(String::from)
                    .ok_or_else(|| {
                        client_error("ClientException", format!("Job queue {q} does not exist"))
                    })?,
            ),
        };
        let mut matched: Vec<&Value> = st
            .map(|s| {
                s.service_jobs
                    .values()
                    .filter(|j| {
                        queue_arn
                            .as_deref()
                            .is_none_or(|q| j.get("jobQueue").and_then(Value::as_str) == Some(q))
                    })
                    .filter(|j| {
                        !status_applies
                            || j.get("status").and_then(Value::as_str) == Some(status.as_str())
                    })
                    .filter(|j| match &filter {
                        None => true,
                        Some((name, values)) => {
                            let v = values[0].as_str();
                            let created = j.get("createdAt").and_then(Value::as_i64).unwrap_or(0);
                            match name.as_str() {
                                "JOB_NAME" => name_filter_matches(
                                    v,
                                    j.get("jobName").and_then(Value::as_str).unwrap_or(""),
                                ),
                                "BEFORE_CREATED_AT" => v.parse::<i64>().is_ok_and(|t| created < t),
                                "AFTER_CREATED_AT" => v.parse::<i64>().is_ok_and(|t| created > t),
                                "SHARE_IDENTIFIER" => {
                                    j.get("shareIdentifier").and_then(Value::as_str) == Some(v)
                                }
                                _ => j.get("quotaShareName").and_then(Value::as_str) == Some(v),
                            }
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
        matched.sort_by(|a, b| {
            let ka = a.get("createdAt").and_then(Value::as_i64).unwrap_or(0);
            let kb = b.get("createdAt").and_then(Value::as_i64).unwrap_or(0);
            kb.cmp(&ka).then_with(|| {
                a.get("jobId")
                    .and_then(Value::as_str)
                    .cmp(&b.get("jobId").and_then(Value::as_str))
            })
        });
        let items: Vec<Value> = matched.into_iter().map(service_job_summary).collect();
        let (page, next) = paginate(items, &body)?;
        let mut resp = Map::new();
        resp.insert("jobSummaryList".into(), Value::Array(page));
        if let Some(n) = next {
            resp.insert("nextToken".into(), json!(n));
        }
        Ok(AwsResponse::ok_json(Value::Object(resp)))
    }

    pub(crate) fn terminate_service_job(
        &self,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        let body = req.json_body();
        let id = required_str(&body, "jobId")?.to_string();
        let reason = required_str(&body, "reason")?.to_string();
        let mut accounts = self.state.write();
        let job = accounts
            .get_or_create(&req.account_id)
            .service_jobs
            .get_mut(&id)
            .and_then(Value::as_object_mut)
            .ok_or_else(|| {
                client_error(
                    "ClientException",
                    format!("Service job {id} does not exist"),
                )
            })?;
        let status = job.get("status").and_then(Value::as_str).unwrap_or("");
        if SERVICE_JOB_ACTIVE.contains(&status) {
            job.insert("status".into(), json!("FAILED"));
            job.insert("statusReason".into(), json!(reason));
            job.insert("isTerminated".into(), json!(true));
            job.insert("stoppedAt".into(), json!(now_ms()));
        }
        Ok(AwsResponse::ok_json(json!({})))
    }

    pub(crate) fn update_service_job(
        &self,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        let body = req.json_body();
        let id = required_str(&body, "jobId")?.to_string();
        let priority = body
            .get("schedulingPriority")
            .and_then(Value::as_i64)
            .ok_or_else(|| client_error("ClientException", "schedulingPriority is required"))?;
        if !(0..=9999).contains(&priority) {
            return Err(client_error(
                "ClientException",
                "schedulingPriority must be between 0 and 9999",
            ));
        }
        let mut accounts = self.state.write();
        let job = accounts
            .get_or_create(&req.account_id)
            .service_jobs
            .get_mut(&id)
            .and_then(Value::as_object_mut)
            .ok_or_else(|| {
                client_error(
                    "ClientException",
                    format!("Service job {id} does not exist"),
                )
            })?;
        let status = job.get("status").and_then(Value::as_str).unwrap_or("");
        if !SERVICE_JOB_ACTIVE.contains(&status) {
            return Err(client_error(
                "ClientException",
                format!("Service job {id} is {status} and can no longer be updated"),
            ));
        }
        job.insert("schedulingPriority".into(), json!(priority));
        Ok(AwsResponse::ok_json(json!({
            "jobArn": job.get("jobArn").cloned().unwrap_or(Value::Null),
            "jobName": job.get("jobName").cloned().unwrap_or(Value::Null),
            "jobId": id,
        })))
    }

    // ---- Job queue snapshot ----

    pub(crate) fn get_job_queue_snapshot(
        &self,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        let body = req.json_body();
        let queue_id = required_str(&body, "jobQueue")?.to_string();
        // Collect what we need under the lock, then resolve container capacity
        // (which re-reads state) after releasing it.
        let (has_quota_policy, has_fair_share, container_jobs, service_jobs) = {
            let accounts = self.state.read();
            let st = accounts.get(&req.account_id);
            let queue = st.and_then(|s| find_queue(s, &queue_id)).ok_or_else(|| {
                client_error(
                    "ClientException",
                    format!("Job queue {queue_id} does not exist"),
                )
            })?;
            let st = st.expect("queue found implies account state");
            let name = queue
                .get("jobQueueName")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let arn = queue
                .get("jobQueueArn")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let policy = queue_policy(st, queue);
            let in_queue = |q: Option<&str>| q == Some(name.as_str()) || q == Some(arn.as_str());
            let container_jobs: Vec<Value> = st
                .jobs
                .values()
                .filter(|j| in_queue(j.get("jobQueue").and_then(Value::as_str)))
                .filter(|j| j.pointer("/arrayProperties/size").is_none())
                .cloned()
                .collect();
            let service_jobs: Vec<Value> = st
                .service_jobs
                .values()
                .filter(|j| in_queue(j.get("jobQueue").and_then(Value::as_str)))
                .cloned()
                .collect();
            (
                policy.is_some_and(|p| p.get("quotaSharePolicy").is_some()),
                policy.is_some_and(|p| p.get("fairsharePolicy").is_some()),
                container_jobs,
                service_jobs,
            )
        };
        let now = now_ms();
        let status_of = |j: &Value| {
            j.get("status")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        };

        // Front of queue: RUNNABLE jobs in dispatch order (priority desc, then
        // the time they reached RUNNABLE).
        let mut runnable: Vec<&Value> = container_jobs
            .iter()
            .chain(service_jobs.iter())
            .filter(|j| status_of(j) == "RUNNABLE")
            .collect();
        let position_time = |j: &Value| {
            j.get("runnableAt")
                .or_else(|| j.get("createdAt"))
                .and_then(Value::as_i64)
                .unwrap_or(0)
        };
        runnable.sort_by(|a, b| {
            let pa = a
                .get("schedulingPriority")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            let pb = b
                .get("schedulingPriority")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            pb.cmp(&pa)
                .then_with(|| position_time(a).cmp(&position_time(b)))
        });
        let front_jobs: Vec<Value> = runnable
            .iter()
            .take(100)
            .map(|j| {
                json!({
                    "jobArn": j.get("jobArn").cloned().unwrap_or(Value::Null),
                    "earliestTimeAtPosition": position_time(j),
                })
            })
            .collect();

        // Dispatched capacity: container jobs in STARTING/RUNNING (VCPU) and
        // service jobs past dispatch (their configured capacity).
        let mut usage: Vec<(Option<String>, Option<String>, String, f64)> = Vec::new();
        for j in &container_jobs {
            if !HOLDING.contains(&status_of(j).as_str()) {
                continue;
            }
            let def = j.get("jobDefinition").and_then(Value::as_str).unwrap_or("");
            let vcpus = self
                .resolve_container(&req.account_id, def, j)
                .map(|c| container_resources(&c).0)
                .unwrap_or(1.0);
            usage.push((
                j.get("shareIdentifier")
                    .and_then(Value::as_str)
                    .map(String::from),
                None,
                "VCPU".to_string(),
                vcpus,
            ));
        }
        for j in &service_jobs {
            if !["SCHEDULED", "STARTING", "RUNNING"].contains(&status_of(j).as_str()) {
                continue;
            }
            for cu in j
                .get("capacityUsage")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                usage.push((
                    j.get("shareIdentifier")
                        .and_then(Value::as_str)
                        .map(String::from),
                    j.get("quotaShareName")
                        .and_then(Value::as_str)
                        .map(String::from),
                    cu.get("capacityUnit")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    cu.get("quantity").and_then(Value::as_f64).unwrap_or(0.0),
                ));
            }
        }
        let sum_by_unit =
            |rows: &mut dyn Iterator<Item = &(Option<String>, Option<String>, String, f64)>| {
                let mut m: BTreeMap<String, f64> = BTreeMap::new();
                for (_, _, unit, q) in rows {
                    *m.entry(unit.clone()).or_default() += q;
                }
                m.into_iter()
                    .map(|(u, q)| json!({ "capacityUnit": u, "quantity": q }))
                    .collect::<Vec<_>>()
            };
        let mut utilization = Map::new();
        utilization.insert(
            "totalCapacityUsage".into(),
            Value::Array(sum_by_unit(&mut usage.iter())),
        );
        if has_fair_share {
            let mut shares: BTreeMap<String, Vec<_>> = BTreeMap::new();
            for row in usage.iter().filter(|r| r.0.is_some()) {
                shares
                    .entry(row.0.clone().unwrap_or_default())
                    .or_default()
                    .push(row);
            }
            let top: Vec<Value> = shares
                .iter()
                .map(|(share, rows)| {
                    json!({
                        "shareIdentifier": share,
                        "capacityUsage": sum_by_unit(&mut rows.iter().copied()),
                    })
                })
                .collect();
            utilization.insert(
                "fairshareUtilization".into(),
                json!({ "activeShareCount": shares.len(), "topCapacityUtilization": top }),
            );
        }
        let mut resp = Map::new();
        resp.insert(
            "frontOfQueue".into(),
            json!({ "jobs": front_jobs, "lastUpdatedAt": now }),
        );
        if has_quota_policy {
            let mut by_share: BTreeMap<String, Vec<_>> = BTreeMap::new();
            for row in usage.iter().filter(|r| r.1.is_some()) {
                by_share
                    .entry(row.1.clone().unwrap_or_default())
                    .or_default()
                    .push(row);
            }
            let top: Vec<Value> = by_share
                .iter()
                .map(|(share, rows)| {
                    json!({
                        "quotaShareName": share,
                        "capacityUsage": sum_by_unit(&mut rows.iter().copied()),
                    })
                })
                .collect();
            utilization.insert(
                "quotaShareUtilization".into(),
                json!({ "topCapacityUtilization": top }),
            );
            // First RUNNABLE job of each quota share.
            let mut first: Map<String, Value> = Map::new();
            for j in runnable.iter() {
                if let Some(share) = j.get("quotaShareName").and_then(Value::as_str) {
                    first.entry(share.to_string()).or_insert_with(|| {
                        json!([{
                            "jobArn": j.get("jobArn").cloned().unwrap_or(Value::Null),
                            "earliestTimeAtPosition": position_time(j),
                        }])
                    });
                }
            }
            resp.insert(
                "frontOfQuotaShares".into(),
                json!({ "quotaShares": first, "lastUpdatedAt": now }),
            );
        }
        utilization.insert("lastUpdatedAt".into(), json!(now));
        resp.insert("queueUtilization".into(), Value::Object(utilization));
        Ok(AwsResponse::ok_json(Value::Object(resp)))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use super::*;
    use crate::state::BatchAccounts;
    use fakecloud_core::service::AwsService;
    use http::{Method, StatusCode};
    use parking_lot::RwLock;

    fn svc() -> BatchService {
        BatchService::new(Arc::new(RwLock::new(BatchAccounts::new())))
    }

    fn req_with(method: Method, path: &str, query: &str, body: Value) -> AwsRequest {
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
            path_segments: fakecloud_core::path::split_path_segments(path),
            raw_path: path.to_string(),
            raw_query: query.to_string(),
            method,
            is_query_protocol: false,
            access_key_id: None,
            principal: None,
        }
    }

    async fn call(s: &BatchService, op: &str, body: Value) -> Result<Value, AwsServiceError> {
        let r = s
            .handle(req_with(Method::POST, &format!("/v1/{op}"), "", body))
            .await?;
        Ok(serde_json::from_slice(r.body.expect_bytes()).unwrap())
    }

    async fn ok(s: &BatchService, op: &str, body: Value) -> Value {
        call(s, op, body)
            .await
            .unwrap_or_else(|e| panic!("{op}: {}", e.message()))
    }

    async fn client_err(s: &BatchService, op: &str, body: Value) -> String {
        match call(s, op, body).await {
            Ok(v) => panic!("{op} should fail, got {v}"),
            Err(e) => {
                assert_eq!(e.status(), StatusCode::BAD_REQUEST);
                assert_eq!(e.code(), "ClientException");
                e.message().to_string()
            }
        }
    }

    /// A deleted resource's tags are purged, and listing them is a
    /// ClientException since the resource no longer exists.
    async fn assert_deleted_tags_gone(s: &BatchService, arn: &str) {
        assert!(!s
            .state
            .read()
            .get("123456789012")
            .unwrap()
            .tags
            .contains_key(arn));
        let path = format!("/v1/tags/{}", arn.replace('/', "%2F"));
        match s.handle(req_with(Method::GET, &path, "", json!({}))).await {
            Err(e) => assert_eq!(e.code(), "ClientException"),
            Ok(_) => panic!("ListTagsForResource on deleted {arn} must be rejected"),
        }
    }

    async fn list_tags(s: &BatchService, arn: &str) -> Value {
        // The SDK percent-encodes the ARN label (its `/` becomes %2F).
        let path = format!("/v1/tags/{}", arn.replace('/', "%2F"));
        let r = s
            .handle(req_with(Method::GET, &path, "", json!({})))
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(r.body.expect_bytes()).unwrap();
        v["tags"].clone()
    }

    #[tokio::test]
    async fn consumable_resource_crud_round_trip() {
        let s = svc();
        let c = ok(
            &s,
            "createconsumableresource",
            json!({"consumableResourceName": "licenses", "totalQuantity": 10,
                   "resourceType": "REPLENISHABLE", "tags": {"team": "ml"}}),
        )
        .await;
        let arn = c["consumableResourceArn"].as_str().unwrap().to_string();
        assert_eq!(
            arn,
            "arn:aws:batch:us-east-1:123456789012:consumable-resource/licenses"
        );
        client_err(
            &s,
            "createconsumableresource",
            json!({"consumableResourceName": "licenses"}),
        )
        .await;

        let d = ok(
            &s,
            "describeconsumableresource",
            json!({"consumableResource": arn}),
        )
        .await;
        assert_eq!(d["totalQuantity"], 10);
        assert_eq!(d["inUseQuantity"], 0);
        assert_eq!(d["availableQuantity"], 10);
        assert_eq!(d["resourceType"], "REPLENISHABLE");
        assert_eq!(d["tags"]["team"], "ml");
        assert!(d["createdAt"].as_i64().unwrap() > 0);
        assert_eq!(list_tags(&s, &arn).await["team"], "ml");

        let u = ok(
            &s,
            "updateconsumableresource",
            json!({"consumableResource": "licenses", "operation": "ADD", "quantity": 5}),
        )
        .await;
        assert_eq!(u["totalQuantity"], 15);
        let u = ok(
            &s,
            "updateconsumableresource",
            json!({"consumableResource": "licenses", "operation": "REMOVE", "quantity": 3}),
        )
        .await;
        assert_eq!(u["totalQuantity"], 12);
        client_err(
            &s,
            "updateconsumableresource",
            json!({"consumableResource": "licenses", "operation": "REMOVE", "quantity": 100}),
        )
        .await;
        // clientToken replay applies once.
        for _ in 0..2 {
            let u = ok(
                &s,
                "updateconsumableresource",
                json!({"consumableResource": "licenses", "operation": "ADD", "quantity": 1,
                       "clientToken": "tok-1"}),
            )
            .await;
            assert_eq!(u["totalQuantity"], 13);
        }
        let u = ok(
            &s,
            "updateconsumableresource",
            json!({"consumableResource": "licenses", "quantity": 4}),
        )
        .await;
        assert_eq!(u["totalQuantity"], 4, "default operation is SET");

        ok(
            &s,
            "createconsumableresource",
            json!({"consumableResourceName": "gpus"}),
        )
        .await;
        let l = ok(
            &s,
            "listconsumableresources",
            json!({"filters": [{"name": "CONSUMABLE_RESOURCE_NAME", "values": ["LIC*"]}]}),
        )
        .await;
        let items = l["consumableResources"].as_array().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["consumableResourceName"], "licenses");
        assert_eq!(items[0]["totalQuantity"], 4);
        let l = ok(&s, "listconsumableresources", json!({"maxResults": 1})).await;
        assert_eq!(l["consumableResources"].as_array().unwrap().len(), 1);
        let next = l["nextToken"].as_str().unwrap().to_string();
        let l = ok(
            &s,
            "listconsumableresources",
            json!({"maxResults": 1, "nextToken": next}),
        )
        .await;
        assert_eq!(l["consumableResources"].as_array().unwrap().len(), 1);
        assert!(l.get("nextToken").is_none());

        ok(
            &s,
            "deleteconsumableresource",
            json!({"consumableResource": "licenses"}),
        )
        .await;
        client_err(
            &s,
            "describeconsumableresource",
            json!({"consumableResource": "licenses"}),
        )
        .await;
        assert_deleted_tags_gone(&s, &arn).await;
    }

    #[tokio::test]
    async fn jobs_listed_by_consumable_resource_and_unknown_resource_rejected() {
        let s = svc();
        ok(
            &s,
            "createconsumableresource",
            json!({"consumableResourceName": "lic", "totalQuantity": 2}),
        )
        .await;
        ok(
            &s,
            "createjobqueue",
            json!({"jobQueueName": "q", "priority": 1}),
        )
        .await;
        ok(
            &s,
            "registerjobdefinition",
            json!({"jobDefinitionName": "jd", "type": "container",
                   "consumableResourceProperties": {"consumableResourceList": [
                       {"consumableResource": "lic", "quantity": 1}]}}),
        )
        .await;
        let j = ok(
            &s,
            "submitjob",
            json!({"jobName": "uses-lic", "jobQueue": "q", "jobDefinition": "jd"}),
        )
        .await;
        ok(
            &s,
            "createjobqueue",
            json!({"jobQueueName": "q2", "priority": 1}),
        )
        .await;
        ok(
            &s,
            "registerjobdefinition",
            json!({"jobDefinitionName": "plain", "type": "container"}),
        )
        .await;
        ok(
            &s,
            "submitjob",
            json!({"jobName": "no-lic", "jobQueue": "q2", "jobDefinition": "plain"}),
        )
        .await;

        let l = ok(
            &s,
            "listjobsbyconsumableresource",
            json!({"consumableResource": "lic"}),
        )
        .await;
        let jobs = l["jobs"].as_array().unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0]["jobArn"], j["jobArn"]);
        assert_eq!(jobs[0]["jobName"], "uses-lic");
        assert_eq!(jobs[0]["quantity"], 1);
        assert_eq!(jobs[0]["jobStatus"], "SUBMITTED");
        assert_eq!(
            jobs[0]["jobQueueArn"],
            "arn:aws:batch:us-east-1:123456789012:job-queue/q"
        );
        assert_eq!(
            jobs[0]["jobDefinitionArn"],
            "arn:aws:batch:us-east-1:123456789012:job-definition/jd:1"
        );
        let filtered = ok(
            &s,
            "listjobsbyconsumableresource",
            json!({"consumableResource": "lic",
                   "filters": [{"name": "JOB_STATUS", "values": ["RUNNING"]}]}),
        )
        .await;
        assert!(filtered["jobs"].as_array().unwrap().is_empty());
        // Without an executor the job never starts, so nothing is in use.
        let d = ok(
            &s,
            "describeconsumableresource",
            json!({"consumableResource": "lic"}),
        )
        .await;
        assert_eq!(d["inUseQuantity"], 0);

        let msg = client_err(
            &s,
            "submitjob",
            json!({"jobName": "bad", "jobQueue": "q", "jobDefinition": "plain",
                   "consumableResourcePropertiesOverride": {"consumableResourceList": [
                       {"consumableResource": "missing", "quantity": 1}]}}),
        )
        .await;
        assert!(msg.contains("missing"));
        client_err(
            &s,
            "listjobsbyconsumableresource",
            json!({"consumableResource": "nope"}),
        )
        .await;
    }

    #[test]
    fn admission_respects_capacity_and_resource_type() {
        let mut st = crate::state::BatchState::default();
        for (name, ty) in [("rep", "REPLENISHABLE"), ("non", "NON_REPLENISHABLE")] {
            st.consumable_resources.insert(
                name.into(),
                json!({"consumableResourceName": name,
                       "consumableResourceArn": format!("arn:aws:batch:us-east-1:1:consumable-resource/{name}"),
                       "totalQuantity": 2, "resourceType": ty}),
            );
        }
        let job = |id: &str, res: &str, status: &str| {
            json!({"jobId": id, "status": status,
                   "consumableResourceProperties": {"consumableResourceList": [
                       {"consumableResource": res, "quantity": 2}]}})
        };
        st.jobs.insert("a".into(), job("a", "rep", "SUBMITTED"));
        st.jobs.insert("b".into(), job("b", "rep", "SUBMITTED"));
        assert!(matches!(admit_job(&mut st, "a"), Admission::Admitted));
        assert_eq!(st.jobs["a"]["status"], "STARTING");
        assert_eq!(consumable_in_use(&st, "rep", None), 2);
        assert!(matches!(admit_job(&mut st, "b"), Admission::Wait));
        assert_eq!(st.jobs["b"]["status"], "RUNNABLE");
        // Replenishable capacity returns when the holder finishes.
        st.jobs.get_mut("a").unwrap()["status"] = json!("SUCCEEDED");
        st.jobs.get_mut("a").unwrap()["startedAt"] = json!(1);
        assert_eq!(consumable_in_use(&st, "rep", None), 0);
        assert!(matches!(admit_job(&mut st, "b"), Admission::Admitted));

        // Non-replenishable capacity stays consumed after the job ends.
        st.jobs.insert("c".into(), job("c", "non", "SUBMITTED"));
        st.jobs.insert("d".into(), job("d", "non", "SUBMITTED"));
        assert!(matches!(admit_job(&mut st, "c"), Admission::Admitted));
        st.jobs.get_mut("c").unwrap()["status"] = json!("SUCCEEDED");
        st.jobs.get_mut("c").unwrap()["startedAt"] = json!(1);
        assert_eq!(consumable_in_use(&st, "non", None), 2);
        assert!(matches!(admit_job(&mut st, "d"), Admission::Wait));
    }

    #[tokio::test]
    async fn retry_relaunch_waits_for_consumable_capacity() {
        let shared: crate::state::SharedBatchState = Arc::new(RwLock::new(BatchAccounts::new()));
        let acct = "123456789012";
        {
            let mut accounts = shared.write();
            let st = accounts.get_or_create(acct);
            st.consumable_resources.insert(
                "lic".into(),
                json!({"consumableResourceName": "lic",
                       "consumableResourceArn": "arn:aws:batch:us-east-1:1:consumable-resource/lic",
                       "totalQuantity": 1, "resourceType": "REPLENISHABLE"}),
            );
            let job = |id: &str, status: &str| {
                json!({"jobId": id, "status": status,
                       "consumableResourceProperties": {"consumableResourceList": [
                           {"consumableResource": "lic", "quantity": 1}]}})
            };
            // "retry" just failed an attempt and is back at RUNNABLE; "other"
            // was admitted in the gap and now holds the only unit.
            st.jobs.insert("retry".into(), job("retry", "RUNNABLE"));
            st.jobs.insert("other".into(), job("other", "RUNNING"));
        }
        let waiter = tokio::spawn({
            let shared = shared.clone();
            async move {
                crate::service::wait_for_retry_admission(&shared, "123456789012", "retry").await
            }
        });
        // Spans two of the waiter's 1s admission polls.
        tokio::time::sleep(std::time::Duration::from_millis(2500)).await;
        assert!(
            !waiter.is_finished(),
            "retry must wait while capacity is held"
        );
        {
            let accounts = shared.read();
            let st = accounts.get(acct).unwrap();
            assert_eq!(st.jobs["retry"]["status"], "RUNNABLE");
            assert_eq!(consumable_in_use(st, "lic", None), 1);
        }
        shared
            .write()
            .get_or_create(acct)
            .jobs
            .get_mut("other")
            .unwrap()["status"] = json!("SUCCEEDED");
        assert!(waiter.await.unwrap());
        let accounts = shared.read();
        let st = accounts.get(acct).unwrap();
        assert_eq!(st.jobs["retry"]["status"], "STARTING");
        assert_eq!(consumable_in_use(st, "lic", None), 1);
    }

    #[tokio::test]
    async fn retry_admission_stops_when_job_terminated() {
        let shared: crate::state::SharedBatchState = Arc::new(RwLock::new(BatchAccounts::new()));
        shared
            .write()
            .get_or_create("123456789012")
            .jobs
            .insert("j".into(), json!({"jobId": "j", "status": "FAILED"}));
        assert!(!crate::service::wait_for_retry_admission(&shared, "123456789012", "j").await);
    }

    #[tokio::test]
    async fn update_job_queue_cannot_mix_environment_orders() {
        let s = svc();
        sagemaker_queue(&s).await;
        ok(
            &s,
            "createcomputeenvironment",
            json!({"computeEnvironmentName": "ce", "type": "UNMANAGED"}),
        )
        .await;
        ok(
            &s,
            "createjobqueue",
            json!({"jobQueueName": "cq", "priority": 1,
                   "computeEnvironmentOrder": [{"order": 1, "computeEnvironment": "ce"}]}),
        )
        .await;
        // A compute queue can't gain a service environment order...
        let msg = client_err(
            &s,
            "updatejobqueue",
            json!({"jobQueue": "cq",
                   "serviceEnvironmentOrder": [{"order": 1, "serviceEnvironment": "se"}]}),
        )
        .await;
        assert!(msg.contains("both"), "{msg}");
        // ...nor a service queue a compute environment order.
        let msg = client_err(
            &s,
            "updatejobqueue",
            json!({"jobQueue": "smq",
                   "computeEnvironmentOrder": [{"order": 1, "computeEnvironment": "ce"}]}),
        )
        .await;
        assert!(msg.contains("both"), "{msg}");
        // Replacing the compute order with a service order sets the type.
        ok(
            &s,
            "updatejobqueue",
            json!({"jobQueue": "cq", "computeEnvironmentOrder": [],
                   "serviceEnvironmentOrder": [{"order": 1, "serviceEnvironment": "se"}]}),
        )
        .await;
        let d = ok(&s, "describejobqueues", json!({"jobQueues": ["cq"]})).await;
        assert_eq!(d["jobQueues"][0]["jobQueueType"], "SAGEMAKER_TRAINING");
        // And back: a service queue switched to a compute order drops the
        // service environment type.
        ok(
            &s,
            "updatejobqueue",
            json!({"jobQueue": "smq", "serviceEnvironmentOrder": [],
                   "computeEnvironmentOrder": [{"order": 1, "computeEnvironment": "ce"}]}),
        )
        .await;
        let d = ok(&s, "describejobqueues", json!({"jobQueues": ["smq"]})).await;
        assert!(d["jobQueues"][0].get("jobQueueType").is_none(), "{d}");
        assert_eq!(
            d["jobQueues"][0]["computeEnvironmentOrder"][0]["computeEnvironment"],
            "ce"
        );
    }

    async fn sagemaker_queue(s: &BatchService) -> String {
        ok(
            s,
            "createserviceenvironment",
            json!({"serviceEnvironmentName": "se", "serviceEnvironmentType": "SAGEMAKER_TRAINING",
                   "capacityLimits": [{"maxCapacity": 4, "capacityUnit": "NUM_INSTANCES"}]}),
        )
        .await;
        let q = ok(
            s,
            "createjobqueue",
            json!({"jobQueueName": "smq", "priority": 1,
                   "serviceEnvironmentOrder": [{"order": 1, "serviceEnvironment": "se"}]}),
        )
        .await;
        q["jobQueueArn"].as_str().unwrap().to_string()
    }

    #[tokio::test]
    async fn service_environment_crud_round_trip() {
        let s = svc();
        let c = ok(
            &s,
            "createserviceenvironment",
            json!({"serviceEnvironmentName": "env1", "serviceEnvironmentType": "SAGEMAKER_TRAINING",
                   "capacityLimits": [{"maxCapacity": 10, "capacityUnit": "NUM_INSTANCES"}],
                   "tags": {"k": "v"}}),
        )
        .await;
        let arn = c["serviceEnvironmentArn"].as_str().unwrap().to_string();
        assert_eq!(
            arn,
            "arn:aws:batch:us-east-1:123456789012:service-environment/env1"
        );
        client_err(
            &s,
            "createserviceenvironment",
            json!({"serviceEnvironmentName": "env2", "serviceEnvironmentType": "ECS",
                   "capacityLimits": [{"maxCapacity": 1, "capacityUnit": "NUM_INSTANCES"}]}),
        )
        .await;
        let d = ok(
            &s,
            "describeserviceenvironments",
            json!({"serviceEnvironments": [arn]}),
        )
        .await;
        let envs = d["serviceEnvironments"].as_array().unwrap();
        assert_eq!(envs.len(), 1);
        assert_eq!(envs[0]["state"], "ENABLED");
        assert_eq!(envs[0]["status"], "VALID");
        assert_eq!(envs[0]["capacityLimits"][0]["maxCapacity"], 10);
        assert_eq!(envs[0]["tags"]["k"], "v");

        let q = ok(
            &s,
            "createjobqueue",
            json!({"jobQueueName": "smq", "priority": 1,
                   "serviceEnvironmentOrder": [{"order": 1, "serviceEnvironment": "env1"}]}),
        )
        .await;
        let qd = ok(&s, "describejobqueues", json!({"jobQueues": ["smq"]})).await;
        assert_eq!(qd["jobQueues"][0]["jobQueueType"], "SAGEMAKER_TRAINING");

        ok(
            &s,
            "updateserviceenvironment",
            json!({"serviceEnvironment": "env1", "state": "DISABLED",
                   "capacityLimits": [{"maxCapacity": 3, "capacityUnit": "NUM_INSTANCES"}]}),
        )
        .await;
        let d = ok(&s, "describeserviceenvironments", json!({})).await;
        assert_eq!(d["serviceEnvironments"][0]["state"], "DISABLED");
        assert_eq!(
            d["serviceEnvironments"][0]["capacityLimits"][0]["maxCapacity"],
            3
        );

        // Still attached to a queue.
        let msg = client_err(
            &s,
            "deleteserviceenvironment",
            json!({"serviceEnvironment": "env1"}),
        )
        .await;
        assert!(msg.contains("smq"));
        ok(&s, "deletejobqueue", json!({"jobQueue": q["jobQueueArn"]})).await;
        ok(
            &s,
            "deleteserviceenvironment",
            json!({"serviceEnvironment": arn}),
        )
        .await;
        let d = ok(&s, "describeserviceenvironments", json!({})).await;
        assert!(d["serviceEnvironments"].as_array().unwrap().is_empty());
        client_err(
            &s,
            "deleteserviceenvironment",
            json!({"serviceEnvironment": "env1"}),
        )
        .await;
    }

    #[tokio::test]
    async fn enabled_service_environment_cannot_be_deleted() {
        let s = svc();
        ok(
            &s,
            "createserviceenvironment",
            json!({"serviceEnvironmentName": "e", "serviceEnvironmentType": "SAGEMAKER_TRAINING",
                   "capacityLimits": [{"maxCapacity": 1, "capacityUnit": "NUM_INSTANCES"}]}),
        )
        .await;
        let msg = client_err(
            &s,
            "deleteserviceenvironment",
            json!({"serviceEnvironment": "e"}),
        )
        .await;
        assert!(msg.contains("DISABLED"));
    }

    fn quota_share_body(name: &str, queue: &str) -> Value {
        json!({"quotaShareName": name, "jobQueue": queue,
               "capacityLimits": [{"maxCapacity": 2, "capacityUnit": "ml.m5.large"}],
               "resourceSharingConfiguration": {"strategy": "LEND_AND_BORROW", "borrowLimit": 50},
               "preemptionConfiguration": {"inSharePreemption": "DISABLED"},
               "tags": {"owner": "a"}})
    }

    #[tokio::test]
    async fn quota_share_crud_round_trip() {
        let s = svc();
        let queue_arn = sagemaker_queue(&s).await;
        let c = ok(&s, "createquotashare", quota_share_body("share1", "smq")).await;
        let arn = c["quotaShareArn"].as_str().unwrap().to_string();
        assert_eq!(c["quotaShareName"], "share1");
        assert_eq!(
            arn,
            "arn:aws:batch:us-east-1:123456789012:job-queue/smq/quota-share/share1"
        );
        client_err(&s, "createquotashare", quota_share_body("share1", "smq")).await;
        client_err(
            &s,
            "createquotashare",
            quota_share_body("x", "no-such-queue"),
        )
        .await;
        let mut bad = quota_share_body("y", "smq");
        bad["resourceSharingConfiguration"] = json!({"strategy": "RESERVE", "borrowLimit": 5});
        client_err(&s, "createquotashare", bad).await;

        let d = ok(&s, "describequotashare", json!({"quotaShareArn": arn})).await;
        assert_eq!(d["jobQueueArn"], queue_arn);
        assert_eq!(d["state"], "ENABLED");
        assert_eq!(d["status"], "VALID");
        assert_eq!(d["capacityLimits"][0]["capacityUnit"], "ml.m5.large");
        assert_eq!(d["resourceSharingConfiguration"]["borrowLimit"], 50);
        assert_eq!(d["tags"]["owner"], "a");
        assert_eq!(list_tags(&s, &arn).await["owner"], "a");

        let l = ok(&s, "listquotashares", json!({"jobQueue": queue_arn})).await;
        assert_eq!(l["quotaShares"].as_array().unwrap().len(), 1);
        assert!(l["quotaShares"][0].get("tags").is_none());

        // Must be disabled before delete.
        client_err(&s, "deletequotashare", json!({"quotaShareArn": arn})).await;
        let u = ok(
            &s,
            "updatequotashare",
            json!({"quotaShareArn": arn, "state": "DISABLED",
                   "capacityLimits": [{"maxCapacity": 8, "capacityUnit": "ml.m5.large"}]}),
        )
        .await;
        assert_eq!(u["quotaShareName"], "share1");
        let d = ok(&s, "describequotashare", json!({"quotaShareArn": arn})).await;
        assert_eq!(d["state"], "DISABLED");
        assert_eq!(d["capacityLimits"][0]["maxCapacity"], 8);
        ok(&s, "deletequotashare", json!({"quotaShareArn": arn})).await;
        client_err(&s, "describequotashare", json!({"quotaShareArn": arn})).await;
        assert_deleted_tags_gone(&s, &arn).await;
    }

    const PAYLOAD: &str = r#"{"TrainingJobName":"t","ResourceConfig":{"InstanceType":"ml.m5.large","InstanceCount":2,"VolumeSizeInGB":10}}"#;

    #[tokio::test]
    async fn service_job_lifecycle() {
        let s = svc();
        let queue_arn = sagemaker_queue(&s).await;
        let j = ok(
            &s,
            "submitservicejob",
            json!({"jobName": "train-1", "jobQueue": "smq", "serviceJobType": "SAGEMAKER_TRAINING",
                   "serviceRequestPayload": PAYLOAD, "schedulingPriority": 5,
                   "retryStrategy": {"attempts": 2}, "timeoutConfig": {"attemptDurationSeconds": 600},
                   "tags": {"exp": "1"}, "clientToken": "ct-1"}),
        )
        .await;
        let id = j["jobId"].as_str().unwrap().to_string();
        let arn = j["jobArn"].as_str().unwrap().to_string();
        assert_eq!(
            arn,
            format!("arn:aws:batch:us-east-1:123456789012:service-job/{id}")
        );
        assert_eq!(list_tags(&s, &arn).await["exp"], "1");

        let d = ok(&s, "describeservicejob", json!({"jobId": id})).await;
        assert_eq!(d["jobName"], "train-1");
        assert_eq!(d["jobQueue"], queue_arn);
        assert_eq!(d["status"], "RUNNABLE");
        assert_eq!(d["isTerminated"], false);
        assert_eq!(d["schedulingPriority"], 5);
        assert_eq!(d["serviceRequestPayload"], PAYLOAD);
        assert_eq!(d["retryStrategy"]["attempts"], 2);
        assert_eq!(d["timeoutConfig"]["attemptDurationSeconds"], 600);
        assert_eq!(d["capacityUsage"][0]["capacityUnit"], "NUM_INSTANCES");
        assert_eq!(d["capacityUsage"][0]["quantity"], 2.0);
        assert_eq!(d["tags"]["exp"], "1");
        assert!(d.get("clientToken").is_none());
        assert!(
            d.get("startedAt").is_none(),
            "never dispatched, never started"
        );

        // Same clientToken is rejected.
        client_err(
            &s,
            "submitservicejob",
            json!({"jobName": "train-1", "jobQueue": "smq", "serviceJobType": "SAGEMAKER_TRAINING",
                   "serviceRequestPayload": PAYLOAD, "clientToken": "ct-1"}),
        )
        .await;

        // Default list status is RUNNING; RUNNABLE must be asked for.
        let l = ok(&s, "listservicejobs", json!({"jobQueue": "smq"})).await;
        assert!(l["jobSummaryList"].as_array().unwrap().is_empty());
        let l = ok(
            &s,
            "listservicejobs",
            json!({"jobQueue": "smq", "jobStatus": "RUNNABLE"}),
        )
        .await;
        assert_eq!(l["jobSummaryList"].as_array().unwrap().len(), 1);
        assert_eq!(
            l["jobSummaryList"][0]["serviceJobType"],
            "SAGEMAKER_TRAINING"
        );
        let l = ok(
            &s,
            "listservicejobs",
            json!({"jobQueue": queue_arn, "filters": [{"name": "JOB_NAME", "values": ["TRAIN*"]}]}),
        )
        .await;
        assert_eq!(l["jobSummaryList"][0]["jobId"], id);

        let u = ok(
            &s,
            "updateservicejob",
            json!({"jobId": id, "schedulingPriority": 42}),
        )
        .await;
        assert_eq!(u["jobArn"], arn);
        let d = ok(&s, "describeservicejob", json!({"jobId": id})).await;
        assert_eq!(d["schedulingPriority"], 42);
        client_err(
            &s,
            "updateservicejob",
            json!({"jobId": id, "schedulingPriority": 10000}),
        )
        .await;

        let snap = ok(&s, "getjobqueuesnapshot", json!({"jobQueue": "smq"})).await;
        assert_eq!(snap["frontOfQueue"]["jobs"][0]["jobArn"], arn);

        ok(
            &s,
            "terminateservicejob",
            json!({"jobId": id, "reason": "user stop"}),
        )
        .await;
        let d = ok(&s, "describeservicejob", json!({"jobId": id})).await;
        assert_eq!(d["status"], "FAILED");
        assert_eq!(d["statusReason"], "user stop");
        assert_eq!(d["isTerminated"], true);
        assert!(d["stoppedAt"].as_i64().is_some());
        client_err(
            &s,
            "updateservicejob",
            json!({"jobId": id, "schedulingPriority": 1}),
        )
        .await;
        let snap = ok(&s, "getjobqueuesnapshot", json!({"jobQueue": "smq"})).await;
        assert!(snap["frontOfQueue"]["jobs"].as_array().unwrap().is_empty());

        client_err(&s, "describeservicejob", json!({"jobId": "nope"})).await;
        client_err(
            &s,
            "terminateservicejob",
            json!({"jobId": "nope", "reason": "x"}),
        )
        .await;
    }

    #[tokio::test]
    async fn submit_service_job_validates_queue_and_payload() {
        let s = svc();
        sagemaker_queue(&s).await;
        ok(
            &s,
            "createjobqueue",
            json!({"jobQueueName": "ecsq", "priority": 1}),
        )
        .await;
        let base = |queue: &str, payload: &str| {
            json!({"jobName": "j", "jobQueue": queue, "serviceJobType": "SAGEMAKER_TRAINING",
                   "serviceRequestPayload": payload})
        };
        client_err(&s, "submitservicejob", base("missing", PAYLOAD)).await;
        let msg = client_err(&s, "submitservicejob", base("ecsq", PAYLOAD)).await;
        assert!(msg.contains("SAGEMAKER_TRAINING"));
        client_err(&s, "submitservicejob", base("smq", "not json")).await;
        let mut with_share = base("smq", PAYLOAD);
        with_share["shareIdentifier"] = json!("a");
        client_err(&s, "submitservicejob", with_share).await;

        // A queue with a quota-share policy requires an existing, enabled share.
        let p = ok(
            &s,
            "createschedulingpolicy",
            json!({"name": "qp", "quotaSharePolicy": {"idleResourceAssignmentStrategy": "FIFO"}}),
        )
        .await;
        ok(
            &s,
            "updatejobqueue",
            json!({"jobQueue": "smq", "schedulingPolicyArn": p["arn"]}),
        )
        .await;
        client_err(&s, "submitservicejob", base("smq", PAYLOAD)).await;
        let mut in_share = base("smq", PAYLOAD);
        in_share["quotaShareName"] = json!("s1");
        client_err(&s, "submitservicejob", in_share.clone()).await;
        let share = ok(&s, "createquotashare", quota_share_body("s1", "smq")).await;
        let j = ok(&s, "submitservicejob", in_share).await;
        let l = ok(
            &s,
            "listservicejobs",
            json!({"jobQueue": "smq", "jobStatus": "RUNNABLE",
                   "filters": [{"name": "QUOTA_SHARE_NAME", "values": ["s1"]}]}),
        )
        .await;
        assert_eq!(l["jobSummaryList"][0]["quotaShareName"], "s1");
        let snap = ok(&s, "getjobqueuesnapshot", json!({"jobQueue": "smq"})).await;
        assert_eq!(
            snap["frontOfQuotaShares"]["quotaShares"]["s1"][0]["jobArn"],
            j["jobArn"]
        );

        // Deleting the (disabled) share terminates its jobs.
        let share_arn = share["quotaShareArn"].clone();
        ok(
            &s,
            "updatequotashare",
            json!({"quotaShareArn": share_arn, "state": "DISABLED"}),
        )
        .await;
        ok(&s, "deletequotashare", json!({"quotaShareArn": share_arn})).await;
        let d = ok(&s, "describeservicejob", json!({"jobId": j["jobId"]})).await;
        assert_eq!(d["status"], "FAILED");
        assert_eq!(d["isTerminated"], true);

        // A disabled queue accepts no new jobs.
        ok(
            &s,
            "updatejobqueue",
            json!({"jobQueue": "smq", "state": "DISABLED"}),
        )
        .await;
        client_err(&s, "submitservicejob", base("smq", PAYLOAD)).await;
    }

    #[tokio::test]
    async fn job_queue_rejects_both_environment_orders_and_unknown_environments() {
        let s = svc();
        client_err(
            &s,
            "createjobqueue",
            json!({"jobQueueName": "q", "priority": 1,
                   "serviceEnvironmentOrder": [{"order": 1, "serviceEnvironment": "ghost"}]}),
        )
        .await;
        sagemaker_queue(&s).await;
        client_err(
            &s,
            "createjobqueue",
            json!({"jobQueueName": "q", "priority": 1,
                   "computeEnvironmentOrder": [{"order": 1, "computeEnvironment": "ce"}],
                   "serviceEnvironmentOrder": [{"order": 1, "serviceEnvironment": "se"}]}),
        )
        .await;
    }

    #[tokio::test]
    async fn new_resources_survive_a_snapshot_round_trip() {
        let s = svc();
        sagemaker_queue(&s).await;
        ok(
            &s,
            "createconsumableresource",
            json!({"consumableResourceName": "r", "totalQuantity": 3}),
        )
        .await;
        ok(&s, "createquotashare", quota_share_body("s", "smq")).await;
        let snapshot = serde_json::to_string(&*s.state.read()).unwrap();
        let restored: BatchAccounts = serde_json::from_str(&snapshot).unwrap();
        let st = restored.get("123456789012").unwrap();
        assert_eq!(st.consumable_resources.len(), 1);
        assert_eq!(st.service_environments.len(), 1);
        assert_eq!(st.quota_shares.len(), 1);
        // Snapshots written before these families existed still load.
        let old: BatchAccounts = serde_json::from_str(r#"{"accounts":{"1":{"jobs":{}}}}"#).unwrap();
        assert!(old.get("1").unwrap().service_jobs.is_empty());
    }
}
