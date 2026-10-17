//! Values SageMaker *derives* when it reports a resource, where the read
//! shape is not just a subset of the write shape the record was stored as.
//! Each transform rewrites a copy of the stored record into the output's
//! terms before it is projected onto the output shape ([`super::shape`]):
//!
//! * `DescribeCluster`: each instance group is reported as
//!   `ClusterInstanceGroupDetails` (requested `InstanceCount` -> current +
//!   target count, `MinInstanceCount` -> `MinCount`, image -> current +
//!   desired image, `InService`), its nested configuration
//!   as current/desired pairs (`ClusterInstanceRequirementDetails`,
//!   `ClusterAutoPatchConfigDetails`, `ClusterKubernetesConfigDetails`,
//!   `ClusterSharedEnvironmentConfigDetails`) and each restricted instance
//!   group as `ClusterRestrictedInstanceGroupDetails` (requested
//!   `InstanceCount` -> current + target count, `InService`). A cluster
//!   update converges immediately, so current and desired are the same.
//! * `DescribeInferenceComponent`: the container's `Image` is reported as the
//!   `DeployedImage` it was deployed from, and `RuntimeConfig.CopyCount` as
//!   the desired + current copy count.
//! * `DescribeTrainingJob`: `ProfilingStatus` follows
//!   `ProfilerConfig.DisableProfiler`; `DescribeFeatureGroup`: an `OnDemand`
//!   group reports no provisioned capacity units.
//! * `DescribeHumanTaskUi`: the stored template `Content` is reported as its
//!   `ContentSha256` (`UiTemplateInfo`).
//! * `DescribeWorkforce` / `UpdateWorkforce` / `ListWorkforces` and the
//!   `Workteam` equivalents: the resource is reported inside its `Workforce`
//!   / `Workteam` structure with its `CreateDate` / `LastUpdatedDate`, and a
//!   work team with the `WorkforceArn` of the workforce it belongs to.

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::generated::OpMeta;

use super::Ctx;

/// The record as the operation's output reports it, when that differs from
/// the stored record.
pub(super) fn record(ctx: &Ctx, meta: &OpMeta, record: &Value) -> Option<Value> {
    let rec = record.as_object()?;
    let mut out = rec.clone();
    match meta.op {
        "DescribeCluster" => cluster(&mut out),
        "DescribeInferenceComponent" => inference_component(&mut out),
        "DescribeHumanTaskUi" => human_task_ui(&mut out),
        "DescribeTrainingJob" => training_job(&mut out),
        "DescribeFeatureGroup" => feature_group(&mut out),
        "DescribeWorkforce" | "UpdateWorkforce" => {
            return Some(wrap("Workforce", workforce(out)));
        }
        "DescribeWorkteam" | "UpdateWorkteam" => {
            return Some(wrap("Workteam", workteam(ctx, out)));
        }
        _ => return None,
    }
    Some(Value::Object(out))
}

/// A list element as the operation's output reports it, when that differs
/// from the stored record.
pub(super) fn element(ctx: &Ctx, meta: &OpMeta, record: &Value) -> Option<Value> {
    let rec = record.as_object()?.clone();
    match meta.op {
        "ListWorkforces" => Some(Value::Object(workforce(rec))),
        "ListWorkteams" => Some(Value::Object(workteam(ctx, rec))),
        _ => None,
    }
}

fn wrap(member: &str, inner: Map<String, Value>) -> Value {
    let mut m = Map::new();
    m.insert(member.to_string(), Value::Object(inner));
    Value::Object(m)
}

/// Report the value stored under `from` as both `current` and `desired`
/// (an applied configuration change has converged), unless the object is
/// already in output form.
fn current_desired(obj: &mut Map<String, Value>, from: &str, current: &str, desired: &str) {
    if obj.contains_key(current) || obj.contains_key(desired) {
        return;
    }
    if let Some(v) = obj.remove(from) {
        obj.insert(current.to_string(), v.clone());
        obj.insert(desired.to_string(), v);
    }
}

fn with_object(obj: &mut Map<String, Value>, key: &str, f: impl FnOnce(&mut Map<String, Value>)) {
    if let Some(inner) = obj.get_mut(key).and_then(Value::as_object_mut) {
        f(inner);
    }
}

fn each_object(
    obj: &mut Map<String, Value>,
    key: &str,
    mut f: impl FnMut(&mut Map<String, Value>),
) {
    if let Some(arr) = obj.get_mut(key).and_then(Value::as_array_mut) {
        for el in arr.iter_mut().filter_map(Value::as_object_mut) {
            f(el);
        }
    }
}

fn cluster(rec: &mut Map<String, Value>) {
    each_object(rec, "InstanceGroups", |group| {
        // `ClusterInstanceGroupSpecification` -> `ClusterInstanceGroupDetails`:
        // the requested count is the group's current and target count, the
        // requested image its current and desired image, and a provisioned
        // group is `InService`. A group already in details form (it carries
        // a `Status`) is left alone.
        if !group.contains_key("Status") {
            if let Some(count) = group.remove("InstanceCount") {
                group.insert("CurrentCount".to_string(), count.clone());
                group.insert("TargetCount".to_string(), count);
            }
            if let Some(min) = group.remove("MinInstanceCount") {
                group.insert("MinCount".to_string(), min);
            }
            current_desired(group, "ImageId", "CurrentImageId", "DesiredImageId");
            current_desired(
                group,
                "ImageReleaseVersion",
                "CurrentImageReleaseVersion",
                "DesiredImageReleaseVersion",
            );
            group.insert("Status".to_string(), Value::String("InService".to_string()));
        }
        with_object(group, "InstanceRequirements", |r| {
            current_desired(
                r,
                "InstanceTypes",
                "CurrentInstanceTypes",
                "DesiredInstanceTypes",
            );
        });
        with_object(group, "AutoPatchConfig", |p| {
            current_desired(
                p,
                "PatchSchedule",
                "CurrentPatchSchedule",
                "DesiredPatchSchedule",
            );
        });
        with_object(group, "KubernetesConfig", |k| {
            current_desired(k, "Labels", "CurrentLabels", "DesiredLabels");
            current_desired(k, "Taints", "CurrentTaints", "DesiredTaints");
        });
    });
    each_object(rec, "RestrictedInstanceGroups", |group| {
        if group.contains_key("Status") {
            return;
        }
        if let Some(count) = group.remove("InstanceCount") {
            group.insert("CurrentCount".to_string(), count.clone());
            group.insert("TargetCount".to_string(), count);
        }
        group.insert("Status".to_string(), Value::String("InService".to_string()));
    });
    with_object(rec, "RestrictedInstanceGroupsConfig", |c| {
        with_object(c, "SharedEnvironmentConfig", |e| {
            current_desired(
                e,
                "FSxLustreConfig",
                "CurrentFSxLustreConfig",
                "DesiredFSxLustreConfig",
            );
            current_desired(
                e,
                "FSxLustreDeletionPolicy",
                "CurrentFSxLustreDeletionPolicy",
                "DesiredFSxLustreDeletionPolicy",
            );
        });
    });
}

fn inference_component(rec: &mut Map<String, Value>) {
    let created = rec.get("CreationTime").cloned();
    let deployed = |spec: &mut Map<String, Value>| {
        with_object(spec, "Container", |c| {
            if c.contains_key("DeployedImage") {
                return;
            }
            let Some(image) = c.remove("Image") else {
                return;
            };
            let mut d = Map::new();
            d.insert("SpecifiedImage".to_string(), image.clone());
            // An image named by digest is already resolved; a tag is resolved
            // against the registry at deploy time, which is not modelled here,
            // so only the specified image is reported for it.
            if image.as_str().is_some_and(|s| s.contains("@sha256:")) {
                d.insert("ResolvedImage".to_string(), image);
                if let Some(t) = created.clone() {
                    d.insert("ResolutionTime".to_string(), t);
                }
            }
            c.insert("DeployedImage".to_string(), Value::Object(d));
        });
    };
    with_object(rec, "Specification", deployed);
    each_object(rec, "Specifications", deployed);
    with_object(rec, "RuntimeConfig", |r| {
        if let Some(n) = r.remove("CopyCount") {
            r.entry("DesiredCopyCount").or_insert_with(|| n.clone());
            r.entry("CurrentCopyCount").or_insert(n);
        }
    });
}

fn human_task_ui(rec: &mut Map<String, Value>) {
    with_object(rec, "UiTemplate", |t| {
        if let Some(content) = t.remove("Content") {
            if let Some(s) = content.as_str() {
                let digest = Sha256::digest(s.as_bytes());
                let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
                t.insert("ContentSha256".to_string(), Value::String(hex));
            }
        }
    });
}

/// `CreateDate` / `LastUpdatedDate` from the record's timestamps.
fn dates(rec: &mut Map<String, Value>) {
    if let Some(t) = rec.get("CreationTime").cloned() {
        rec.entry("CreateDate").or_insert(t);
    }
    if let Some(t) = rec.get("LastModifiedTime").cloned() {
        rec.entry("LastUpdatedDate").or_insert(t);
    }
}

fn workforce(mut rec: Map<String, Value>) -> Map<String, Value> {
    dates(&mut rec);
    rec.entry("Status")
        .or_insert_with(|| Value::String("Active".to_string()));
    rec
}

fn workteam(ctx: &Ctx, mut rec: Map<String, Value>) -> Map<String, Value> {
    dates(&mut rec);
    if !rec.contains_key("WorkforceArn") {
        // A work team belongs to the named private workforce, else the
        // account's `default` one.
        let workforce = rec
            .get("WorkforceName")
            .and_then(Value::as_str)
            .unwrap_or("default")
            .to_string();
        rec.insert(
            "WorkforceArn".to_string(),
            Value::String(super::mint_arn(ctx, "workforce", &workforce)),
        );
    }
    rec
}

/// `ProfilingStatus` follows `ProfilerConfig.DisableProfiler` (set at create
/// or by `UpdateTrainingJob`): `true` turns Debugger profiling off.
fn training_job(rec: &mut Map<String, Value>) {
    let Some(config) = rec.get("ProfilerConfig").and_then(Value::as_object) else {
        return;
    };
    let disabled = config.get("DisableProfiler").and_then(Value::as_bool) == Some(true);
    let status = if disabled { "Disabled" } else { "Enabled" };
    rec.insert(
        "ProfilingStatus".to_string(),
        Value::String(status.to_string()),
    );
}

/// Provisioned read / write capacity units do not apply to an `OnDemand`
/// feature group, so they are not reported once the group is (or has been
/// switched to) on-demand.
fn feature_group(rec: &mut Map<String, Value>) {
    with_object(rec, "ThroughputConfig", |t| {
        if t.get("ThroughputMode").and_then(Value::as_str) == Some("OnDemand") {
            t.remove("ProvisionedReadCapacityUnits");
            t.remove("ProvisionedWriteCapacityUnits");
        }
    });
}
