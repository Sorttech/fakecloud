//! CloudFormation StackSets.
//!
//! A stack set is a template plus parameters that is deployed as one stack per
//! (account, region) pair, a *stack instance*. Every call that changes what is
//! deployed runs as a stack set *operation*. The instances an operation touches
//! are provisioned by driving the ordinary CreateStack / UpdateStack /
//! DeleteStack paths in the target account and region, so an instance's stack
//! is a real stack whose resources exist in the backing services. Each
//! target's outcome is recorded on the operation, which is what
//! DescribeStackSetOperation and ListStackSetOperationResults report.
//!
//! Operations run to completion inside the call that starts them, except for
//! stacks that provision asynchronously (templates with custom resources).
//! Those leave the instance `RUNNING`, and every later read of the stack set
//! folds the stack's current status back into the instance and the operation.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use fakecloud_aws::xml::xml_escape;
use fakecloud_core::multi_account::MultiAccountState;
use fakecloud_core::service::{AwsRequest, AwsResponse, AwsServiceError};

use crate::extras::{looks_like_url, xml_response, xml_response_no_result};
use crate::service::CloudFormationService;
use crate::state::CloudFormationState;

/// Service principal Organizations uses for StackSets trusted access and
/// delegated administration.
const STACKSETS_PRINCIPAL: &str = "member.org.stacksets.cloudformation.amazonaws.com";
/// Name of the optional per-account Lambda that gates deployments.
const ACCOUNT_GATE_FUNCTION: &str = "AWSCloudFormationStackSetAccountGate";
const DEFAULT_ADMIN_ROLE: &str = "AWSCloudFormationStackSetAdministrationRole";
const DEFAULT_EXECUTION_ROLE: &str = "AWSCloudFormationStackSetExecutionRole";
/// ImportStacksToStackSet accepts at most this many stacks per call.
const MAX_IMPORT_STACKS: usize = 10;
const DEFAULT_PAGE_SIZE: usize = 100;

const TOLERANCE_EXCEEDED: &str = "Cancelled since failure tolerance has exceeded";
const OPERATION_STOPPED: &str = "Cancelled since the operation was stopped";

// ── State ──

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StackSet {
    pub stack_set_id: String,
    pub name: String,
    pub arn: String,
    /// `ACTIVE`, or `DELETED` once DeleteStackSet has run. Deleted stack sets
    /// stay listable and describable by id, as in AWS.
    pub status: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub template_body: String,
    #[serde(default)]
    pub parameters: BTreeMap<String, String>,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub tags: Vec<(String, String)>,
    #[serde(default)]
    pub administration_role_arn: Option<String>,
    #[serde(default)]
    pub execution_role_name: Option<String>,
    pub permission_model: String,
    #[serde(default)]
    pub auto_deployment: Option<AutoDeployment>,
    #[serde(default)]
    pub managed_execution_active: bool,
    #[serde(default)]
    pub instances: Vec<StackInstance>,
    #[serde(default)]
    pub operations: Vec<StackSetOperation>,
    /// Result of the most recent DetectStackSetDrift, if any.
    #[serde(default)]
    pub drift: Option<DriftDetectionDetails>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutoDeployment {
    pub enabled: bool,
    pub retain_stacks_on_account_removal: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StackInstance {
    pub account: String,
    pub region: String,
    #[serde(default)]
    pub stack_id: Option<String>,
    /// `CURRENT`, `OUTDATED` or `INOPERABLE`.
    pub status: String,
    /// `StackInstanceStatus.DetailedStatus`.
    pub detailed_status: String,
    #[serde(default)]
    pub status_reason: Option<String>,
    #[serde(default)]
    pub parameter_overrides: BTreeMap<String, String>,
    #[serde(default)]
    pub organizational_unit_id: Option<String>,
    pub drift_status: String,
    #[serde(default)]
    pub last_drift_check_timestamp: Option<DateTime<Utc>>,
    #[serde(default)]
    pub last_operation_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StackSetOperation {
    pub operation_id: String,
    /// `CREATE`, `UPDATE`, `DELETE` or `DETECT_DRIFT`.
    pub action: String,
    /// `RUNNING`, `SUCCEEDED`, `FAILED`, `STOPPING` or `STOPPED`.
    pub status: String,
    #[serde(default)]
    pub status_reason: Option<String>,
    #[serde(default)]
    pub retain_stacks: Option<bool>,
    #[serde(default)]
    pub preferences: OperationPreferences,
    #[serde(default)]
    pub deployment_targets: Option<DeploymentTargets>,
    #[serde(default)]
    pub administration_role_arn: Option<String>,
    #[serde(default)]
    pub execution_role_name: Option<String>,
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub ended_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub results: Vec<OperationResult>,
    #[serde(default)]
    pub drift: Option<DriftDetectionDetails>,
    #[serde(default)]
    pub resource_drifts: Vec<InstanceResourceDrift>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperationResult {
    pub account: String,
    pub region: String,
    /// `PENDING`, `RUNNING`, `SUCCEEDED`, `FAILED` or `CANCELLED`.
    pub status: String,
    #[serde(default)]
    pub status_reason: Option<String>,
    #[serde(default)]
    pub organizational_unit_id: Option<String>,
    #[serde(default)]
    pub account_gate_status: Option<String>,
    #[serde(default)]
    pub account_gate_reason: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OperationPreferences {
    #[serde(default)]
    pub region_concurrency_type: Option<String>,
    #[serde(default)]
    pub region_order: Vec<String>,
    #[serde(default)]
    pub failure_tolerance_count: Option<u32>,
    #[serde(default)]
    pub failure_tolerance_percentage: Option<u32>,
    #[serde(default)]
    pub max_concurrent_count: Option<u32>,
    #[serde(default)]
    pub max_concurrent_percentage: Option<u32>,
    #[serde(default)]
    pub concurrency_mode: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DeploymentTargets {
    #[serde(default)]
    pub accounts: Vec<String>,
    #[serde(default)]
    pub accounts_url: Option<String>,
    #[serde(default)]
    pub organizational_unit_ids: Vec<String>,
    #[serde(default)]
    pub account_filter_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DriftDetectionDetails {
    pub drift_status: String,
    pub detection_status: String,
    pub last_drift_check_timestamp: DateTime<Utc>,
    pub total: usize,
    pub drifted: usize,
    pub in_sync: usize,
    pub failed: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstanceResourceDrift {
    pub account: String,
    pub region: String,
    pub stack_id: String,
    pub logical_id: String,
    pub physical_id: String,
    pub resource_type: String,
    /// `IN_SYNC`, `DELETED` or `NOT_CHECKED`.
    pub status: String,
    pub timestamp: DateTime<Utc>,
}

/// Move stack sets persisted by older builds, which kept a
/// `{StackSetId, StackSetName, Status, TemplateBody}` JSON record in the
/// generic `extras` store, into the typed store.
pub fn migrate_legacy_stack_sets(accounts: &mut MultiAccountState<CloudFormationState>) {
    for (account_id, state) in accounts.iter_mut() {
        let Some(legacy) = state.extras.remove("stack_sets") else {
            continue;
        };
        for (name, record) in legacy {
            let id = record["StackSetId"]
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| format!("{name}:{}", uuid::Uuid::new_v4()));
            if state.stack_sets.contains_key(&id) {
                continue;
            }
            let arn = format!(
                "arn:aws:cloudformation:{}:{account_id}:stackset/{id}",
                state.region
            );
            state.stack_sets.insert(
                id.clone(),
                StackSet {
                    stack_set_id: id,
                    name: record["StackSetName"].as_str().unwrap_or(&name).to_string(),
                    arn,
                    status: record["Status"].as_str().unwrap_or("ACTIVE").to_string(),
                    description: None,
                    template_body: record["TemplateBody"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                    parameters: BTreeMap::new(),
                    capabilities: Vec::new(),
                    tags: Vec::new(),
                    administration_role_arn: None,
                    execution_role_name: None,
                    permission_model: "SELF_MANAGED".to_string(),
                    auto_deployment: None,
                    managed_execution_active: false,
                    instances: Vec::new(),
                    operations: Vec::new(),
                    drift: None,
                    created_at: Utc::now(),
                },
            );
        }
    }
}

pub(crate) fn is_stack_set_action(action: &str) -> bool {
    matches!(
        action,
        "CreateStackSet"
            | "DescribeStackSet"
            | "ListStackSets"
            | "UpdateStackSet"
            | "DeleteStackSet"
            | "CreateStackInstances"
            | "UpdateStackInstances"
            | "DeleteStackInstances"
            | "DescribeStackInstance"
            | "ListStackInstances"
            | "DescribeStackSetOperation"
            | "ListStackSetOperations"
            | "ListStackSetOperationResults"
            | "StopStackSetOperation"
            | "ImportStacksToStackSet"
            | "ListStackSetAutoDeploymentTargets"
            | "DetectStackSetDrift"
            | "ListStackInstanceResourceDrifts"
    )
}

/// Which stack sets a caller can address. A StackSets delegated administrator
/// acts on the management account's stack sets, but only the service-managed
/// ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Scope {
    Own,
    DelegatedAdmin,
}

impl Scope {
    fn of(params: &BTreeMap<String, String>) -> Self {
        if params.get("CallAs").map(String::as_str) == Some("DELEGATED_ADMIN") {
            Scope::DelegatedAdmin
        } else {
            Scope::Own
        }
    }

    fn sees(self, set: &StackSet) -> bool {
        self == Scope::Own || set.permission_model == "SERVICE_MANAGED"
    }
}

/// Find an ACTIVE stack set by name or id.
pub(crate) fn find_active<'a>(
    state: &'a CloudFormationState,
    name_or_id: &str,
    scope: Scope,
) -> Option<&'a StackSet> {
    state.stack_sets.values().find(|s| {
        s.status == "ACTIVE"
            && (s.name == name_or_id || s.stack_set_id == name_or_id)
            && scope.sees(s)
    })
}

/// Find a stack set for a read: an active one by name or id, or a deleted one
/// by its (unique) id. A deleted stack set's name is free for reuse, so a name
/// never resolves to one.
fn find_for_read<'a>(
    state: &'a CloudFormationState,
    name_or_id: &str,
    scope: Scope,
) -> Option<&'a StackSet> {
    find_active(state, name_or_id, scope)
        .or_else(|| state.stack_sets.get(name_or_id).filter(|s| scope.sees(s)))
}

fn active_key(state: &CloudFormationState, name_or_id: &str, scope: Scope) -> Option<String> {
    find_active(state, name_or_id, scope).map(|s| s.stack_set_id.clone())
}

// ── Errors ──

fn aws_err(status: StatusCode, code: &str, message: impl Into<String>) -> AwsServiceError {
    AwsServiceError::aws_error(status, code, message)
}

fn validation(message: impl Into<String>) -> AwsServiceError {
    aws_err(StatusCode::BAD_REQUEST, "ValidationError", message)
}

fn stack_set_not_found(name: &str) -> AwsServiceError {
    aws_err(
        StatusCode::NOT_FOUND,
        "StackSetNotFoundException",
        format!("StackSet {name} not found"),
    )
}

fn operation_not_found(op_id: &str) -> AwsServiceError {
    aws_err(
        StatusCode::NOT_FOUND,
        "OperationNotFoundException",
        format!("Operation {op_id} not found"),
    )
}

fn instance_not_found(set: &str, account: &str, region: &str) -> AwsServiceError {
    aws_err(
        StatusCode::NOT_FOUND,
        "StackInstanceNotFoundException",
        format!("Stack instance with account {account} and region {region} not found for stack set {set}"),
    )
}

fn required(params: &BTreeMap<String, String>, field: &str) -> Result<String, AwsServiceError> {
    params
        .get(field)
        .cloned()
        .ok_or_else(|| validation(format!("{field} is required")))
}

// ── Request parsing ──

/// `Prefix.member.N` scalar list.
fn member_list(params: &BTreeMap<String, String>, prefix: &str) -> Vec<String> {
    (1..)
        .map_while(|i| params.get(&format!("{prefix}.member.{i}")).cloned())
        .collect()
}

/// Whether the request carries the list `prefix` at all, including the empty
/// form (`Prefix=`) the CLI sends for an explicitly empty list.
fn list_present(params: &BTreeMap<String, String>, prefix: &str) -> bool {
    let dotted = format!("{prefix}.");
    params.contains_key(prefix) || params.keys().any(|k| k.starts_with(&dotted))
}

struct ParameterEntry {
    key: String,
    value: Option<String>,
    use_previous: bool,
}

fn parameter_list(params: &BTreeMap<String, String>, prefix: &str) -> Vec<ParameterEntry> {
    (1..)
        .map_while(|i| {
            let key = params.get(&format!("{prefix}.member.{i}.ParameterKey"))?;
            Some(ParameterEntry {
                key: key.clone(),
                value: params
                    .get(&format!("{prefix}.member.{i}.ParameterValue"))
                    .cloned(),
                use_previous: params
                    .get(&format!("{prefix}.member.{i}.UsePreviousValue"))
                    .is_some_and(|v| v.eq_ignore_ascii_case("true")),
            })
        })
        .collect()
}

/// Resolve a parameter list against `previous`: explicit values win, and
/// `UsePreviousValue` entries carry the previous value forward.
fn resolve_parameters(
    entries: &[ParameterEntry],
    previous: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>, AwsServiceError> {
    let mut out = BTreeMap::new();
    for entry in entries {
        match (&entry.value, entry.use_previous) {
            (Some(_), true) => {
                return Err(validation(format!(
                    "Invalid input for parameter key {}. Cannot specify usePreviousValue as true and a parameter value at the same time",
                    entry.key
                )))
            }
            (Some(value), false) => {
                out.insert(entry.key.clone(), value.clone());
            }
            (None, true) => match previous.get(&entry.key) {
                Some(value) => {
                    out.insert(entry.key.clone(), value.clone());
                }
                None => {
                    return Err(validation(format!(
                        "Parameter {} does not have a previous value",
                        entry.key
                    )))
                }
            },
            (None, false) => {
                return Err(validation(format!(
                    "Invalid input for parameter key {}. Need to specify either usePreviousValue as true or a value for the parameter",
                    entry.key
                )))
            }
        }
    }
    Ok(out)
}

fn tag_list(params: &BTreeMap<String, String>) -> Vec<(String, String)> {
    (1..)
        .map_while(|i| {
            let key = params.get(&format!("Tags.member.{i}.Key"))?;
            let value = params.get(&format!("Tags.member.{i}.Value"))?;
            Some((key.clone(), value.clone()))
        })
        .collect()
}

fn parse_bool(
    params: &BTreeMap<String, String>,
    key: &str,
) -> Result<Option<bool>, AwsServiceError> {
    match params.get(key) {
        None => Ok(None),
        Some(v) if v.eq_ignore_ascii_case("true") => Ok(Some(true)),
        Some(v) if v.eq_ignore_ascii_case("false") => Ok(Some(false)),
        Some(v) => Err(validation(format!("Invalid value {v} for {key}"))),
    }
}

fn parse_u32(params: &BTreeMap<String, String>, key: &str) -> Result<Option<u32>, AwsServiceError> {
    params
        .get(key)
        .map(|v| {
            v.parse::<u32>()
                .map_err(|_| validation(format!("Invalid value {v} for {key}")))
        })
        .transpose()
}

fn parse_preferences(
    params: &BTreeMap<String, String>,
) -> Result<OperationPreferences, AwsServiceError> {
    let p = "OperationPreferences";
    let prefs = OperationPreferences {
        region_concurrency_type: params.get(&format!("{p}.RegionConcurrencyType")).cloned(),
        region_order: member_list(params, &format!("{p}.RegionOrder")),
        failure_tolerance_count: parse_u32(params, &format!("{p}.FailureToleranceCount"))?,
        failure_tolerance_percentage: parse_u32(
            params,
            &format!("{p}.FailureTolerancePercentage"),
        )?,
        max_concurrent_count: parse_u32(params, &format!("{p}.MaxConcurrentCount"))?,
        max_concurrent_percentage: parse_u32(params, &format!("{p}.MaxConcurrentPercentage"))?,
        concurrency_mode: params.get(&format!("{p}.ConcurrencyMode")).cloned(),
    };
    if prefs.failure_tolerance_count.is_some() && prefs.failure_tolerance_percentage.is_some() {
        return Err(validation(
            "FailureToleranceCount and FailureTolerancePercentage cannot both be specified",
        ));
    }
    if prefs.max_concurrent_count.is_some() && prefs.max_concurrent_percentage.is_some() {
        return Err(validation(
            "MaxConcurrentCount and MaxConcurrentPercentage cannot both be specified",
        ));
    }
    if prefs.failure_tolerance_percentage.is_some_and(|v| v > 100)
        || prefs.max_concurrent_percentage.is_some_and(|v| v > 100)
    {
        return Err(validation("Percentage values must be between 0 and 100"));
    }
    Ok(prefs)
}

fn parse_deployment_targets(params: &BTreeMap<String, String>) -> Option<DeploymentTargets> {
    let p = "DeploymentTargets";
    if !list_present(params, p) {
        return None;
    }
    Some(DeploymentTargets {
        accounts: member_list(params, &format!("{p}.Accounts")),
        accounts_url: params.get(&format!("{p}.AccountsUrl")).cloned(),
        organizational_unit_ids: member_list(params, &format!("{p}.OrganizationalUnitIds")),
        account_filter_type: params.get(&format!("{p}.AccountFilterType")).cloned(),
    })
}

/// The deployment targets an operation records, in `DeploymentTargets` form
/// whether the request used top-level `Accounts` or `DeploymentTargets`.
fn targets_record(
    accounts: &[String],
    deployment_targets: Option<&DeploymentTargets>,
) -> DeploymentTargets {
    match deployment_targets {
        Some(dt) => dt.clone(),
        None => DeploymentTargets {
            accounts: accounts.to_vec(),
            ..DeploymentTargets::default()
        },
    }
}

fn is_account_id(value: &str) -> bool {
    value.len() == 12 && value.bytes().all(|b| b.is_ascii_digit())
}

/// Parse `arn:aws:cloudformation:{region}:{account}:stack/{name}/{id}` into
/// `(account, region)`.
fn stack_arn_location(arn: &str) -> Option<(String, String)> {
    let parts: Vec<&str> = arn.splitn(6, ':').collect();
    if parts.len() != 6 || parts[0] != "arn" || parts[2] != "cloudformation" {
        return None;
    }
    if !parts[5].starts_with("stack/") || !is_account_id(parts[4]) || parts[3].is_empty() {
        return None;
    }
    Some((parts[4].to_string(), parts[3].to_string()))
}

fn paginate<T>(
    items: Vec<T>,
    params: &BTreeMap<String, String>,
) -> Result<(Vec<T>, Option<String>), AwsServiceError> {
    let start = match params.get("NextToken") {
        Some(token) => token
            .parse::<usize>()
            .map_err(|_| validation("Invalid NextToken"))?,
        None => 0,
    };
    let size = params
        .get("MaxResults")
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_PAGE_SIZE);
    let total = items.len();
    if start > total {
        return Err(validation("Invalid NextToken"));
    }
    let end = start.saturating_add(size);
    let page: Vec<T> = items.into_iter().skip(start).take(size).collect();
    let next = (end < total).then(|| end.to_string());
    Ok((page, next))
}

// ── XML ──

fn ts(t: &DateTime<Utc>) -> String {
    t.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

fn el(name: &str, value: &str) -> String {
    format!("<{name}>{}</{name}>", xml_escape(value))
}

fn opt_el(name: &str, value: Option<&str>) -> String {
    value.map(|v| el(name, v)).unwrap_or_default()
}

fn list_el(name: &str, members: impl IntoIterator<Item = String>) -> String {
    let inner: String = members
        .into_iter()
        .map(|m| format!("<member>{m}</member>"))
        .collect();
    if inner.is_empty() {
        format!("<{name}/>")
    } else {
        format!("<{name}>{inner}</{name}>")
    }
}

fn scalar_list_el<'a>(name: &str, values: impl IntoIterator<Item = &'a String>) -> String {
    list_el(name, values.into_iter().map(|v| xml_escape(v)))
}

fn parameters_el(name: &str, params: &BTreeMap<String, String>) -> String {
    list_el(
        name,
        params
            .iter()
            .map(|(k, v)| format!("{}{}", el("ParameterKey", k), el("ParameterValue", v))),
    )
}

fn next_token_el(next: Option<String>) -> String {
    next.map(|t| el("NextToken", &t)).unwrap_or_default()
}

fn preferences_el(p: &OperationPreferences) -> String {
    let mut out = String::new();
    out.push_str(&opt_el(
        "RegionConcurrencyType",
        p.region_concurrency_type.as_deref(),
    ));
    if !p.region_order.is_empty() {
        out.push_str(&scalar_list_el("RegionOrder", &p.region_order));
    }
    for (name, value) in [
        ("FailureToleranceCount", p.failure_tolerance_count),
        ("FailureTolerancePercentage", p.failure_tolerance_percentage),
        ("MaxConcurrentCount", p.max_concurrent_count),
        ("MaxConcurrentPercentage", p.max_concurrent_percentage),
    ] {
        if let Some(v) = value {
            out.push_str(&el(name, &v.to_string()));
        }
    }
    out.push_str(&opt_el("ConcurrencyMode", p.concurrency_mode.as_deref()));
    format!("<OperationPreferences>{out}</OperationPreferences>")
}

fn drift_details_el(d: Option<&DriftDetectionDetails>) -> String {
    match d {
        Some(d) => format!(
            "<StackSetDriftDetectionDetails>{}{}{}{}{}{}{}{}</StackSetDriftDetectionDetails>",
            el("DriftStatus", &d.drift_status),
            el("DriftDetectionStatus", &d.detection_status),
            el("LastDriftCheckTimestamp", &ts(&d.last_drift_check_timestamp)),
            el("TotalStackInstancesCount", &d.total.to_string()),
            el("DriftedStackInstancesCount", &d.drifted.to_string()),
            el("InSyncStackInstancesCount", &d.in_sync.to_string()),
            el("InProgressStackInstancesCount", "0"),
            el("FailedStackInstancesCount", &d.failed.to_string()),
        ),
        None => "<StackSetDriftDetectionDetails><DriftStatus>NOT_CHECKED</DriftStatus><TotalStackInstancesCount>0</TotalStackInstancesCount><DriftedStackInstancesCount>0</DriftedStackInstancesCount><InSyncStackInstancesCount>0</InSyncStackInstancesCount><InProgressStackInstancesCount>0</InProgressStackInstancesCount><FailedStackInstancesCount>0</FailedStackInstancesCount></StackSetDriftDetectionDetails>".to_string(),
    }
}

fn auto_deployment_el(a: Option<&AutoDeployment>) -> String {
    a.map(|a| {
        format!(
            "<AutoDeployment>{}{}</AutoDeployment>",
            el("Enabled", &a.enabled.to_string()),
            el(
                "RetainStacksOnAccountRemoval",
                &a.retain_stacks_on_account_removal.to_string()
            ),
        )
    })
    .unwrap_or_default()
}

fn managed_execution_el(active: bool) -> String {
    format!(
        "<ManagedExecution>{}</ManagedExecution>",
        el("Active", &active.to_string())
    )
}

fn deployment_targets_el(t: &DeploymentTargets) -> String {
    let mut out = String::new();
    if !t.accounts.is_empty() {
        out.push_str(&scalar_list_el("Accounts", &t.accounts));
    }
    out.push_str(&opt_el("AccountsUrl", t.accounts_url.as_deref()));
    if !t.organizational_unit_ids.is_empty() {
        out.push_str(&scalar_list_el(
            "OrganizationalUnitIds",
            &t.organizational_unit_ids,
        ));
    }
    out.push_str(&opt_el(
        "AccountFilterType",
        t.account_filter_type.as_deref(),
    ));
    format!("<DeploymentTargets>{out}</DeploymentTargets>")
}

fn status_details_el(op: &StackSetOperation) -> String {
    let failed = op.results.iter().filter(|r| r.status == "FAILED").count();
    format!(
        "<StatusDetails>{}</StatusDetails>",
        el("FailedStackInstancesCount", &failed.to_string())
    )
}

fn stack_set_regions(set: &StackSet) -> Vec<String> {
    let regions: BTreeSet<&String> = set.instances.iter().map(|i| &i.region).collect();
    regions.into_iter().cloned().collect()
}

fn stack_set_ous(set: &StackSet) -> Vec<String> {
    let ous: BTreeSet<&String> = set
        .instances
        .iter()
        .filter_map(|i| i.organizational_unit_id.as_ref())
        .collect();
    ous.into_iter().cloned().collect()
}

fn stack_set_el(set: &StackSet) -> String {
    let mut out = String::new();
    out.push_str(&el("StackSetName", &set.name));
    out.push_str(&el("StackSetId", &set.stack_set_id));
    out.push_str(&opt_el("Description", set.description.as_deref()));
    out.push_str(&el("Status", &set.status));
    out.push_str(&el("TemplateBody", &set.template_body));
    out.push_str(&parameters_el("Parameters", &set.parameters));
    out.push_str(&scalar_list_el("Capabilities", &set.capabilities));
    out.push_str(&list_el(
        "Tags",
        set.tags
            .iter()
            .map(|(k, v)| format!("{}{}", el("Key", k), el("Value", v))),
    ));
    out.push_str(&el("StackSetARN", &set.arn));
    out.push_str(&opt_el(
        "AdministrationRoleARN",
        set.administration_role_arn.as_deref(),
    ));
    out.push_str(&opt_el(
        "ExecutionRoleName",
        set.execution_role_name.as_deref(),
    ));
    out.push_str(&drift_details_el(set.drift.as_ref()));
    out.push_str(&auto_deployment_el(set.auto_deployment.as_ref()));
    out.push_str(&el("PermissionModel", &set.permission_model));
    out.push_str(&scalar_list_el(
        "OrganizationalUnitIds",
        &stack_set_ous(set),
    ));
    out.push_str(&managed_execution_el(set.managed_execution_active));
    out.push_str(&scalar_list_el("Regions", &stack_set_regions(set)));
    format!("<StackSet>{out}</StackSet>")
}

fn stack_set_summary_el(set: &StackSet) -> String {
    let mut out = String::new();
    out.push_str(&el("StackSetName", &set.name));
    out.push_str(&el("StackSetId", &set.stack_set_id));
    out.push_str(&opt_el("Description", set.description.as_deref()));
    out.push_str(&el("Status", &set.status));
    out.push_str(&auto_deployment_el(set.auto_deployment.as_ref()));
    out.push_str(&el("PermissionModel", &set.permission_model));
    out.push_str(&el(
        "DriftStatus",
        set.drift
            .as_ref()
            .map_or("NOT_CHECKED", |d| d.drift_status.as_str()),
    ));
    if let Some(d) = &set.drift {
        out.push_str(&el(
            "LastDriftCheckTimestamp",
            &ts(&d.last_drift_check_timestamp),
        ));
    }
    out.push_str(&managed_execution_el(set.managed_execution_active));
    out
}

fn instance_fields(set: &StackSet, i: &StackInstance, with_overrides: bool) -> String {
    let mut out = String::new();
    out.push_str(&el("StackSetId", &set.stack_set_id));
    out.push_str(&el("Region", &i.region));
    out.push_str(&el("Account", &i.account));
    out.push_str(&opt_el("StackId", i.stack_id.as_deref()));
    if with_overrides {
        out.push_str(&parameters_el("ParameterOverrides", &i.parameter_overrides));
    }
    out.push_str(&el("Status", &i.status));
    out.push_str(&format!(
        "<StackInstanceStatus>{}</StackInstanceStatus>",
        el("DetailedStatus", &i.detailed_status)
    ));
    out.push_str(&opt_el("StatusReason", i.status_reason.as_deref()));
    out.push_str(&opt_el(
        "OrganizationalUnitId",
        i.organizational_unit_id.as_deref(),
    ));
    out.push_str(&el("DriftStatus", &i.drift_status));
    if let Some(t) = &i.last_drift_check_timestamp {
        out.push_str(&el("LastDriftCheckTimestamp", &ts(t)));
    }
    out.push_str(&opt_el("LastOperationId", i.last_operation_id.as_deref()));
    out
}

fn operation_el(set: &StackSet, op: &StackSetOperation) -> String {
    let mut out = String::new();
    out.push_str(&el("OperationId", &op.operation_id));
    out.push_str(&el("StackSetId", &set.stack_set_id));
    out.push_str(&el("Action", &op.action));
    out.push_str(&el("Status", &op.status));
    out.push_str(&preferences_el(&op.preferences));
    if let Some(retain) = op.retain_stacks {
        out.push_str(&el("RetainStacks", &retain.to_string()));
    }
    out.push_str(&opt_el(
        "AdministrationRoleARN",
        op.administration_role_arn.as_deref(),
    ));
    out.push_str(&opt_el(
        "ExecutionRoleName",
        op.execution_role_name.as_deref(),
    ));
    out.push_str(&el("CreationTimestamp", &ts(&op.created_at)));
    if let Some(t) = &op.ended_at {
        out.push_str(&el("EndTimestamp", &ts(t)));
    }
    if let Some(t) = &op.deployment_targets {
        out.push_str(&deployment_targets_el(t));
    }
    if op.drift.is_some() {
        out.push_str(&drift_details_el(op.drift.as_ref()));
    }
    out.push_str(&opt_el("StatusReason", op.status_reason.as_deref()));
    out.push_str(&status_details_el(op));
    format!("<StackSetOperation>{out}</StackSetOperation>")
}

fn operation_summary_el(op: &StackSetOperation) -> String {
    let mut out = String::new();
    out.push_str(&el("OperationId", &op.operation_id));
    out.push_str(&el("Action", &op.action));
    out.push_str(&el("Status", &op.status));
    out.push_str(&el("CreationTimestamp", &ts(&op.created_at)));
    if let Some(t) = &op.ended_at {
        out.push_str(&el("EndTimestamp", &ts(t)));
    }
    out.push_str(&opt_el("StatusReason", op.status_reason.as_deref()));
    out.push_str(&status_details_el(op));
    out.push_str(&preferences_el(&op.preferences));
    out
}

fn operation_result_el(r: &OperationResult) -> String {
    let mut out = String::new();
    out.push_str(&el("Account", &r.account));
    out.push_str(&el("Region", &r.region));
    out.push_str(&el("Status", &r.status));
    out.push_str(&opt_el("StatusReason", r.status_reason.as_deref()));
    if let Some(gate) = &r.account_gate_status {
        out.push_str(&format!(
            "<AccountGateResult>{}{}</AccountGateResult>",
            el("Status", gate),
            opt_el("StatusReason", r.account_gate_reason.as_deref())
        ));
    }
    out.push_str(&opt_el(
        "OrganizationalUnitId",
        r.organizational_unit_id.as_deref(),
    ));
    out
}

// ── Operation engine ──

/// One (account, region) an operation acts on.
#[derive(Debug, Clone)]
struct Target {
    account: String,
    region: String,
    ou: Option<String>,
    suspended: bool,
}

/// What an operation does to each target.
#[derive(Debug, Clone)]
enum TargetAction {
    /// Create the instance (or re-deploy it, when it already exists) with
    /// these overrides.
    Create {
        overrides: BTreeMap<String, String>,
    },
    /// Re-deploy the instance's stack. `None` keeps the instance's overrides.
    Update {
        overrides: Option<Vec<OverrideSpec>>,
    },
    Delete {
        retain_stacks: bool,
    },
}

#[derive(Debug, Clone)]
enum OverrideSpec {
    Value(String, String),
    UsePrevious(String),
}

#[derive(Debug, Clone, PartialEq)]
enum Outcome {
    Succeeded,
    Running,
    Failed(String),
    Cancelled(String),
    SkippedSuspended,
}

struct GateResult {
    status: &'static str,
    reason: Option<String>,
}

/// The stack-set fields an operation deploys, captured when it starts.
#[derive(Clone)]
struct DeploySpec {
    name: String,
    template_body: String,
    parameters: BTreeMap<String, String>,
    capabilities: Vec<String>,
    tags: Vec<(String, String)>,
}

impl DeploySpec {
    fn of(set: &StackSet) -> Self {
        Self {
            name: set.name.clone(),
            template_body: set.template_body.clone(),
            parameters: set.parameters.clone(),
            capabilities: set.capabilities.clone(),
            tags: set.tags.clone(),
        }
    }

    fn stack_params(&self, overrides: &BTreeMap<String, String>) -> Vec<(String, String)> {
        let mut merged = self.parameters.clone();
        merged.extend(overrides.iter().map(|(k, v)| (k.clone(), v.clone())));
        let mut out = vec![("TemplateBody".to_string(), self.template_body.clone())];
        for (i, (k, v)) in merged.iter().enumerate() {
            out.push((
                format!("Parameters.member.{}.ParameterKey", i + 1),
                k.clone(),
            ));
            out.push((
                format!("Parameters.member.{}.ParameterValue", i + 1),
                v.clone(),
            ));
        }
        for (i, cap) in self.capabilities.iter().enumerate() {
            out.push((format!("Capabilities.member.{}", i + 1), cap.clone()));
        }
        for (i, (k, v)) in self.tags.iter().enumerate() {
            out.push((format!("Tags.member.{}.Key", i + 1), k.clone()));
            out.push((format!("Tags.member.{}.Value", i + 1), v.clone()));
        }
        out
    }
}

/// Map a stack's status after an instance operation to that target's outcome.
fn stack_outcome(action: &str, status: &str, reason: Option<&str>) -> Outcome {
    if status.ends_with("_IN_PROGRESS") {
        return Outcome::Running;
    }
    let ok = match action {
        "DELETE" => status == "DELETE_COMPLETE",
        _ => matches!(
            status,
            "CREATE_COMPLETE" | "UPDATE_COMPLETE" | "IMPORT_COMPLETE"
        ),
    };
    if ok {
        Outcome::Succeeded
    } else {
        Outcome::Failed(
            reason
                .map(str::to_string)
                .unwrap_or_else(|| format!("Stack is in {status} state")),
        )
    }
}

fn result_status(outcome: &Outcome) -> &'static str {
    match outcome {
        Outcome::Succeeded => "SUCCEEDED",
        Outcome::Running => "RUNNING",
        Outcome::Failed(_) => "FAILED",
        Outcome::Cancelled(_) | Outcome::SkippedSuspended => "CANCELLED",
    }
}

fn outcome_reason(outcome: &Outcome) -> Option<String> {
    match outcome {
        Outcome::Failed(r) | Outcome::Cancelled(r) => Some(r.clone()),
        Outcome::SkippedSuspended => Some("Account is suspended".to_string()),
        _ => None,
    }
}

/// Apply an outcome to the instance it concerns.
fn apply_to_instance(instance: &mut StackInstance, outcome: &Outcome) {
    let (status, detailed) = match outcome {
        Outcome::Succeeded => ("CURRENT", "SUCCEEDED"),
        Outcome::Running => ("OUTDATED", "RUNNING"),
        Outcome::Failed(_) => ("OUTDATED", "FAILED"),
        Outcome::Cancelled(_) => ("OUTDATED", "CANCELLED"),
        Outcome::SkippedSuspended => ("OUTDATED", "SKIPPED_SUSPENDED_ACCOUNT"),
    };
    instance.status = status.to_string();
    instance.detailed_status = detailed.to_string();
    instance.status_reason = outcome_reason(outcome);
}

/// Allowed failures per region before an operation stops.
fn region_tolerance(prefs: &OperationPreferences, accounts_in_region: usize) -> usize {
    match (
        prefs.failure_tolerance_count,
        prefs.failure_tolerance_percentage,
    ) {
        (Some(count), _) => count as usize,
        (None, Some(pct)) => accounts_in_region * pct as usize / 100,
        (None, None) => 0,
    }
}

/// Final status of an operation none of whose targets are still running.
fn settled_status(op: &StackSetOperation) -> &'static str {
    if op.status == "STOPPING" || op.status == "STOPPED" {
        return "STOPPED";
    }
    let mut per_region: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
    for r in &op.results {
        let entry = per_region.entry(r.region.as_str()).or_default();
        entry.0 += 1;
        if r.status == "FAILED" {
            entry.1 += 1;
        }
    }
    let exceeded = per_region
        .values()
        .any(|(total, failed)| *failed > region_tolerance(&op.preferences, *total));
    if exceeded {
        "FAILED"
    } else {
        "SUCCEEDED"
    }
}

fn settle_operation(op: &mut StackSetOperation) {
    if !matches!(op.status.as_str(), "RUNNING" | "STOPPING") {
        return;
    }
    if op
        .results
        .iter()
        .any(|r| matches!(r.status.as_str(), "RUNNING" | "PENDING"))
    {
        return;
    }
    op.status = settled_status(op).to_string();
    op.ended_at = Some(Utc::now());
}

/// Order targets the way the operation deploys them: `RegionOrder` first,
/// then the remaining regions in request order.
fn order_targets(
    mut targets: Vec<Target>,
    regions: &[String],
    prefs: &OperationPreferences,
) -> Vec<Target> {
    let mut order: Vec<&String> = prefs.region_order.iter().collect();
    for r in regions {
        if !order.contains(&r) {
            order.push(r);
        }
    }
    targets.sort_by_key(|t| {
        order
            .iter()
            .position(|r| **r == t.region)
            .unwrap_or(usize::MAX)
    });
    targets
}

fn synthetic_request(
    account: &str,
    region: &str,
    action: &str,
    request_id: &str,
    params: Vec<(String, String)>,
) -> AwsRequest {
    let mut query: std::collections::HashMap<String, String> = params.into_iter().collect();
    query.insert("Action".to_string(), action.to_string());
    AwsRequest {
        service: "cloudformation".to_string(),
        action: action.to_string(),
        region: region.to_string(),
        account_id: account.to_string(),
        request_id: request_id.to_string(),
        headers: http::HeaderMap::new(),
        query_params: query,
        body: bytes::Bytes::new(),
        body_stream: parking_lot::Mutex::new(None),
        path_segments: Vec::new(),
        raw_path: "/".to_string(),
        raw_query: String::new(),
        method: http::Method::POST,
        is_query_protocol: true,
        access_key_id: None,
        principal: None,
    }
}

/// The accounts in (or below) an OU, or the whole organization for the root.
fn accounts_under(
    org: &fakecloud_organizations::OrganizationState,
    ou: &str,
) -> Vec<(String, bool)> {
    let mut out = Vec::new();
    for account in org.accounts.values() {
        if account.id == org.management_account_id {
            // Service-managed stack sets never deploy to the management account.
            continue;
        }
        let mut parent = account.parent_id.clone();
        let mut depth = 0;
        loop {
            if parent == ou {
                out.push((account.id.clone(), account.status != "ACTIVE"));
                break;
            }
            match org.ous.get(&parent) {
                Some(p) if depth < 16 => {
                    parent = p.parent_id.clone();
                    depth += 1;
                }
                _ => break,
            }
        }
    }
    out
}

impl CloudFormationService {
    pub(crate) async fn handle_stack_set_action(
        &self,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        let params = Self::get_all_params(req);
        match req.action.as_str() {
            "CreateStackSet" => self.create_stack_set(req, &params),
            "DescribeStackSet" => self.describe_stack_set(req, &params),
            "ListStackSets" => self.list_stack_sets(req, &params),
            "UpdateStackSet" => self.update_stack_set(req, &params).await,
            "DeleteStackSet" => self.delete_stack_set(req, &params),
            "CreateStackInstances" => self.create_stack_instances(req, &params).await,
            "UpdateStackInstances" => self.update_stack_instances(req, &params).await,
            "DeleteStackInstances" => self.delete_stack_instances(req, &params).await,
            "DescribeStackInstance" => self.describe_stack_instance(req, &params),
            "ListStackInstances" => self.list_stack_instances(req, &params),
            "DescribeStackSetOperation" => self.describe_stack_set_operation(req, &params),
            "ListStackSetOperations" => self.list_stack_set_operations(req, &params),
            "ListStackSetOperationResults" => self.list_stack_set_operation_results(req, &params),
            "StopStackSetOperation" => self.stop_stack_set_operation(req, &params),
            "ImportStacksToStackSet" => self.import_stacks_to_stack_set(req, &params),
            "ListStackSetAutoDeploymentTargets" => {
                self.list_stack_set_auto_deployment_targets(req, &params)
            }
            "DetectStackSetDrift" => self.detect_stack_set_drift(req, &params),
            "ListStackInstanceResourceDrifts" => {
                self.list_stack_instance_resource_drifts(req, &params)
            }
            other => Err(validation(format!("Unsupported stack set action {other}"))),
        }
    }

    /// The account whose stack sets a call addresses. `CallAs=DELEGATED_ADMIN`
    /// lets a registered StackSets delegated administrator act on the
    /// organization's service-managed stack sets, which live in the
    /// management account.
    fn stack_set_admin_account(
        &self,
        req: &AwsRequest,
        params: &BTreeMap<String, String>,
    ) -> Result<String, AwsServiceError> {
        match params.get("CallAs").map(String::as_str) {
            None | Some("SELF") => Ok(req.account_id.clone()),
            Some("DELEGATED_ADMIN") => {
                let orgs = self.deps.organizations.read();
                let org = orgs.as_ref().ok_or_else(|| {
                    validation("AWS Organizations is not enabled for this account")
                })?;
                let registered = org
                    .delegated_administrators
                    .get(STACKSETS_PRINCIPAL)
                    .is_some_and(|admins| admins.contains_key(&req.account_id));
                if !registered {
                    return Err(validation(format!(
                        "Account {} is not registered as a delegated administrator for {STACKSETS_PRINCIPAL}",
                        req.account_id
                    )));
                }
                Ok(org.management_account_id.clone())
            }
            Some(other) => Err(validation(format!("Invalid value {other} for CallAs"))),
        }
    }

    /// Service-managed stack sets need an organization with StackSets trusted
    /// access, administered from its management account.
    fn check_service_managed_allowed(&self, admin: &str) -> Result<(), AwsServiceError> {
        let trusted_in_org = {
            let orgs = self.deps.organizations.read();
            let org = orgs
                .as_ref()
                .ok_or_else(|| validation("AWS Organizations is not enabled for this account"))?;
            if org.management_account_id != admin {
                return Err(validation(
                    "Service managed stack sets can only be administered from the organization's management account or a delegated administrator",
                ));
            }
            org.trusted_services.contains_key(STACKSETS_PRINCIPAL)
        };
        let activated = self
            .state
            .read()
            .get(admin)
            .is_some_and(|s| s.orgs_access_enabled);
        if trusted_in_org || activated {
            Ok(())
        } else {
            Err(validation(
                "You must enable organizations access to operate a service managed stack set",
            ))
        }
    }

    // ── Stack sets ──

    fn create_stack_set(
        &self,
        req: &AwsRequest,
        params: &BTreeMap<String, String>,
    ) -> Result<AwsResponse, AwsServiceError> {
        let name = required(params, "StackSetName")?;
        let admin = self.stack_set_admin_account(req, params)?;
        let permission_model = params
            .get("PermissionModel")
            .cloned()
            .unwrap_or_else(|| "SELF_MANAGED".to_string());
        let service_managed = permission_model == "SERVICE_MANAGED";
        if Scope::of(params) == Scope::DelegatedAdmin && !service_managed {
            return Err(validation(
                "A delegated administrator can only create stack sets with SERVICE_MANAGED permission model",
            ));
        }
        if service_managed {
            self.check_service_managed_allowed(&admin)?;
        }

        // A stack set can be created from an existing stack, which is how
        // ImportStacksToStackSet adoption starts.
        let (template_body, mut parameters) = match params.get("StackId") {
            Some(stack_id) => {
                let accounts = self.state.read();
                let stack = accounts
                    .get(&admin)
                    .and_then(|s| {
                        s.stacks
                            .values()
                            .find(|st| &st.stack_id == stack_id && st.status != "DELETE_COMPLETE")
                    })
                    .ok_or_else(|| {
                        validation(format!("Stack with id {stack_id} does not exist"))
                    })?;
                let params: BTreeMap<String, String> = stack
                    .parameters
                    .iter()
                    .filter(|(k, _)| !k.starts_with("AWS::"))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                (stack.template.clone(), params)
            }
            None => (
                self.stack_set_template_body(&admin, params)
                    .map_err(validation)?
                    .unwrap_or_default(),
                BTreeMap::new(),
            ),
        };
        parameters.extend(resolve_parameters(
            &parameter_list(params, "Parameters"),
            &BTreeMap::new(),
        )?);

        let auto_deployment = Self::parse_auto_deployment(params, service_managed, None)?;
        let managed_execution_active =
            parse_bool(params, "ManagedExecution.Active")?.unwrap_or(false);
        let (administration_role_arn, execution_role_name) = if service_managed {
            (None, None)
        } else {
            (
                Some(
                    params
                        .get("AdministrationRoleARN")
                        .cloned()
                        .unwrap_or_else(|| {
                            format!("arn:aws:iam::{admin}:role/{DEFAULT_ADMIN_ROLE}")
                        }),
                ),
                Some(
                    params
                        .get("ExecutionRoleName")
                        .cloned()
                        .unwrap_or_else(|| DEFAULT_EXECUTION_ROLE.to_string()),
                ),
            )
        };

        let id = format!("{name}:{}", uuid::Uuid::new_v4());
        let arn = format!(
            "arn:aws:cloudformation:{}:{admin}:stackset/{id}",
            req.region
        );
        let mut accounts = self.state.write();
        let state = accounts.get_or_create(&admin);
        if find_active(state, &name, Scope::Own).is_some() {
            return Err(aws_err(
                StatusCode::CONFLICT,
                "NameAlreadyExistsException",
                format!("StackSet {name} already exists"),
            ));
        }
        state.stack_sets.insert(
            id.clone(),
            StackSet {
                stack_set_id: id.clone(),
                name,
                arn,
                status: "ACTIVE".to_string(),
                description: params.get("Description").cloned(),
                template_body,
                parameters,
                capabilities: member_list(params, "Capabilities"),
                tags: tag_list(params),
                administration_role_arn,
                execution_role_name,
                permission_model,
                auto_deployment,
                managed_execution_active,
                instances: Vec::new(),
                operations: Vec::new(),
                drift: None,
                created_at: Utc::now(),
            },
        );
        Ok(xml_response(
            "CreateStackSet",
            el("StackSetId", &id),
            &req.request_id,
        ))
    }

    fn parse_auto_deployment(
        params: &BTreeMap<String, String>,
        service_managed: bool,
        previous: Option<&AutoDeployment>,
    ) -> Result<Option<AutoDeployment>, AwsServiceError> {
        let enabled = parse_bool(params, "AutoDeployment.Enabled")?;
        let retain = parse_bool(params, "AutoDeployment.RetainStacksOnAccountRemoval")?;
        if enabled.is_none() && retain.is_none() {
            return Ok(previous.cloned());
        }
        if !service_managed {
            return Err(validation(
                "AutoDeployment is only supported for stack sets with SERVICE_MANAGED permission model",
            ));
        }
        let enabled = enabled.or(previous.map(|p| p.enabled)).unwrap_or(false);
        let retain = retain
            .or(previous.map(|p| p.retain_stacks_on_account_removal))
            .unwrap_or(false);
        if retain && !enabled {
            return Err(validation(
                "RetainStacksOnAccountRemoval can only be set when AutoDeployment is enabled",
            ));
        }
        Ok(Some(AutoDeployment {
            enabled,
            retain_stacks_on_account_removal: retain,
        }))
    }

    /// Fold asynchronously-provisioning stacks' current status into the
    /// instances and operations of one stack set.
    fn refresh_stack_set(
        accounts: &mut MultiAccountState<CloudFormationState>,
        admin: &str,
        set_id: &str,
    ) {
        let running: Vec<(usize, String, String, Option<String>)> =
            match accounts.get(admin).and_then(|s| s.stack_sets.get(set_id)) {
                Some(set) => set
                    .instances
                    .iter()
                    .enumerate()
                    .filter(|(_, i)| i.detailed_status == "RUNNING")
                    .filter_map(|(idx, i)| {
                        Some((
                            idx,
                            i.account.clone(),
                            i.stack_id.clone()?,
                            i.last_operation_id.clone(),
                        ))
                    })
                    .collect(),
                None => return,
            };
        let mut outcomes = Vec::new();
        for (idx, account, stack_id, op_id) in running {
            let Some((status, reason)) = accounts.get(&account).and_then(|s| {
                s.stacks
                    .values()
                    .find(|st| st.stack_id == stack_id)
                    .map(|st| (st.status.clone(), st.status_reason.clone()))
            }) else {
                outcomes.push((
                    idx,
                    op_id,
                    Outcome::Failed(format!("Stack {stack_id} does not exist")),
                ));
                continue;
            };
            let action = if status.starts_with("UPDATE") {
                "UPDATE"
            } else {
                "CREATE"
            };
            let outcome = stack_outcome(action, &status, reason.as_deref());
            if outcome != Outcome::Running {
                outcomes.push((idx, op_id, outcome));
            }
        }
        let Some(set) = accounts
            .get_mut(admin)
            .and_then(|s| s.stack_sets.get_mut(set_id))
        else {
            return;
        };
        for (idx, op_id, outcome) in outcomes {
            let (account, region) = {
                let instance = &mut set.instances[idx];
                apply_to_instance(instance, &outcome);
                (instance.account.clone(), instance.region.clone())
            };
            if let Some(op) = op_id
                .as_deref()
                .and_then(|id| set.operations.iter_mut().find(|o| o.operation_id == id))
            {
                if let Some(result) = op
                    .results
                    .iter_mut()
                    .find(|r| r.account == account && r.region == region)
                {
                    result.status = result_status(&outcome).to_string();
                    result.status_reason = outcome_reason(&outcome);
                }
            }
        }
        for op in &mut set.operations {
            settle_operation(op);
        }
    }

    /// Resolve the stack set for a read, after folding in async progress.
    fn read_stack_set(
        &self,
        admin: &str,
        name_or_id: &str,
        scope: Scope,
    ) -> Result<StackSet, AwsServiceError> {
        let mut accounts = self.state.write();
        let id = accounts
            .get(admin)
            .and_then(|s| find_for_read(s, name_or_id, scope))
            .map(|s| s.stack_set_id.clone())
            .ok_or_else(|| stack_set_not_found(name_or_id))?;
        Self::refresh_stack_set(&mut accounts, admin, &id);
        accounts
            .get(admin)
            .and_then(|s| s.stack_sets.get(&id))
            .cloned()
            .ok_or_else(|| stack_set_not_found(name_or_id))
    }

    fn describe_stack_set(
        &self,
        req: &AwsRequest,
        params: &BTreeMap<String, String>,
    ) -> Result<AwsResponse, AwsServiceError> {
        let name = required(params, "StackSetName")?;
        let admin = self.stack_set_admin_account(req, params)?;
        let set = self.read_stack_set(&admin, &name, Scope::of(params))?;
        Ok(xml_response(
            "DescribeStackSet",
            stack_set_el(&set),
            &req.request_id,
        ))
    }

    fn list_stack_sets(
        &self,
        req: &AwsRequest,
        params: &BTreeMap<String, String>,
    ) -> Result<AwsResponse, AwsServiceError> {
        let admin = self.stack_set_admin_account(req, params)?;
        let wanted = params.get("Status");
        let mut sets: Vec<StackSet> = self
            .state
            .read()
            .get(&admin)
            .map(|s| {
                s.stack_sets
                    .values()
                    .filter(|set| wanted.is_none_or(|w| &set.status == w))
                    .filter(|set| Scope::of(params).sees(set))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        sets.sort_by_key(|a| a.created_at);
        let (page, next) = paginate(sets, params)?;
        let inner = format!(
            "{}{}",
            list_el("Summaries", page.iter().map(stack_set_summary_el)),
            next_token_el(next)
        );
        Ok(xml_response("ListStackSets", inner, &req.request_id))
    }

    /// Reject a new operation on a stack set that already has one running, and
    /// a caller-supplied operation id that was used before.
    fn check_can_start_operation(set: &StackSet, op_id: &str) -> Result<(), AwsServiceError> {
        if set
            .operations
            .iter()
            .any(|o| matches!(o.status.as_str(), "RUNNING" | "STOPPING"))
        {
            return Err(aws_err(
                StatusCode::CONFLICT,
                "OperationInProgressException",
                format!(
                    "Another Operation on StackSet {} is in progress",
                    set.stack_set_id
                ),
            ));
        }
        if set.operations.iter().any(|o| o.operation_id == op_id) {
            return Err(aws_err(
                StatusCode::CONFLICT,
                "OperationIdAlreadyExistsException",
                format!("Operation {op_id} already exists"),
            ));
        }
        Ok(())
    }

    fn new_operation(
        set: &StackSet,
        op_id: &str,
        action: &str,
        preferences: OperationPreferences,
        deployment_targets: Option<DeploymentTargets>,
        retain_stacks: Option<bool>,
    ) -> StackSetOperation {
        StackSetOperation {
            operation_id: op_id.to_string(),
            action: action.to_string(),
            status: "RUNNING".to_string(),
            status_reason: None,
            retain_stacks,
            preferences,
            deployment_targets,
            administration_role_arn: set.administration_role_arn.clone(),
            execution_role_name: set.execution_role_name.clone(),
            created_at: Utc::now(),
            ended_at: None,
            results: Vec::new(),
            drift: None,
            resource_drifts: Vec::new(),
        }
    }

    async fn update_stack_set(
        &self,
        req: &AwsRequest,
        params: &BTreeMap<String, String>,
    ) -> Result<AwsResponse, AwsServiceError> {
        let name = required(params, "StackSetName")?;
        let admin = self.stack_set_admin_account(req, params)?;
        let preferences = parse_preferences(params)?;
        let op_id = params
            .get("OperationId")
            .cloned()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        // Resolved before the state lock: a TemplateURL read takes the S3 lock.
        let new_template = self
            .stack_set_template_body(&admin, params)
            .map_err(validation)?;
        let use_previous_template = parse_bool(params, "UsePreviousTemplate")?.unwrap_or(false);
        if use_previous_template && new_template.is_some() {
            return Err(validation(
                "UsePreviousTemplate cannot be specified together with TemplateBody or TemplateURL",
            ));
        }
        let explicit_regions = member_list(params, "Regions");
        let explicit_accounts = member_list(params, "Accounts");
        let deployment_targets = parse_deployment_targets(params);

        // Build the updated definition and resolve targets against a snapshot,
        // without holding the CloudFormation lock while Organizations and S3
        // are read. The snapshot is re-validated under the lock below.
        let snapshot = self.active_snapshot(&admin, &name, Scope::of(params))?;
        Self::check_can_start_operation(&snapshot, &op_id)?;
        let mut updated = snapshot.clone();
        if let Some(body) = new_template {
            updated.template_body = body;
        }
        let entries = parameter_list(params, "Parameters");
        if !entries.is_empty() {
            updated.parameters = resolve_parameters(&entries, &snapshot.parameters)?;
        }
        if let Some(description) = params.get("Description") {
            updated.description = Some(description.clone());
        }
        if list_present(params, "Capabilities") {
            updated.capabilities = member_list(params, "Capabilities");
        }
        if list_present(params, "Tags") {
            updated.tags = tag_list(params);
        }
        if let Some(model) = params.get("PermissionModel") {
            if *model != snapshot.permission_model {
                if !snapshot.instances.is_empty() {
                    return Err(validation(
                        "PermissionModel cannot be changed for a stack set that has stack instances",
                    ));
                }
                updated.permission_model = model.clone();
            }
        }
        let service_managed = updated.permission_model == "SERVICE_MANAGED";
        if Scope::of(params) == Scope::DelegatedAdmin && !service_managed {
            return Err(validation(
                "A delegated administrator can only manage stack sets with SERVICE_MANAGED permission model",
            ));
        }
        if service_managed && snapshot.permission_model != "SERVICE_MANAGED" {
            self.check_service_managed_allowed(&admin)?;
        }
        if service_managed {
            updated.administration_role_arn = None;
            updated.execution_role_name = None;
        } else {
            if let Some(role) = params.get("AdministrationRoleARN") {
                updated.administration_role_arn = Some(role.clone());
            }
            if let Some(role) = params.get("ExecutionRoleName") {
                updated.execution_role_name = Some(role.clone());
            }
            updated
                .administration_role_arn
                .get_or_insert_with(|| format!("arn:aws:iam::{admin}:role/{DEFAULT_ADMIN_ROLE}"));
            updated
                .execution_role_name
                .get_or_insert_with(|| DEFAULT_EXECUTION_ROLE.to_string());
        }
        updated.auto_deployment = Self::parse_auto_deployment(
            params,
            service_managed,
            snapshot.auto_deployment.as_ref(),
        )?;
        if let Some(active) = parse_bool(params, "ManagedExecution.Active")? {
            updated.managed_execution_active = active;
        }

        // Which instances this update re-deploys: those named by
        // Accounts/DeploymentTargets + Regions, or all of them.
        let targeted = !explicit_accounts.is_empty() || deployment_targets.is_some();
        if targeted && explicit_regions.is_empty() {
            return Err(validation(
                "Regions must be specified when Accounts or DeploymentTargets are specified",
            ));
        }
        if !targeted && !explicit_regions.is_empty() {
            return Err(validation(
                "Accounts or DeploymentTargets must be specified when Regions are specified",
            ));
        }
        let (targets, regions) = if targeted {
            let targets = self.existing_instance_targets(
                &updated,
                &admin,
                &explicit_accounts,
                deployment_targets.as_ref(),
                &explicit_regions,
                true,
            )?;
            (targets, explicit_regions.clone())
        } else {
            let targets = updated
                .instances
                .iter()
                .map(|i| Target {
                    account: i.account.clone(),
                    region: i.region.clone(),
                    ou: i.organizational_unit_id.clone(),
                    suspended: false,
                })
                .collect();
            (targets, stack_set_regions(&updated))
        };
        // Instances left out of a partial update fall behind the new stack
        // set definition.
        for instance in &mut updated.instances {
            if !targets
                .iter()
                .any(|t| t.account == instance.account && t.region == instance.region)
            {
                instance.status = "OUTDATED".to_string();
            }
        }
        let targets = order_targets(targets, &regions, &preferences);
        let record =
            targeted.then(|| targets_record(&explicit_accounts, deployment_targets.as_ref()));
        let op = Self::new_operation(&updated, &op_id, "UPDATE", preferences, record, None);
        updated.operations.push(op);
        let spec = DeploySpec::of(&updated);
        let set_id = updated.stack_set_id.clone();
        {
            let mut accounts = self.state.write();
            Self::refresh_stack_set(&mut accounts, &admin, &set_id);
            let current = accounts
                .get(&admin)
                .and_then(|s| s.stack_sets.get(&set_id))
                .filter(|s| s.status == "ACTIVE")
                .ok_or_else(|| stack_set_not_found(&name))?;
            Self::check_not_stale(current, &snapshot)?;
            Self::check_can_start_operation(current, &op_id)?;
            accounts
                .get_or_create(&admin)
                .stack_sets
                .insert(set_id.clone(), updated);
        }

        self.run_operation(
            req,
            &admin,
            &set_id,
            &op_id,
            &spec,
            targets,
            TargetAction::Update { overrides: None },
        )
        .await;
        Ok(xml_response(
            "UpdateStackSet",
            el("OperationId", &op_id),
            &req.request_id,
        ))
    }

    /// A clone of the ACTIVE stack set `name`, with async progress folded in.
    fn active_snapshot(
        &self,
        admin: &str,
        name: &str,
        scope: Scope,
    ) -> Result<StackSet, AwsServiceError> {
        let mut accounts = self.state.write();
        let set_id = accounts
            .get(admin)
            .and_then(|s| active_key(s, name, scope))
            .ok_or_else(|| stack_set_not_found(name))?;
        Self::refresh_stack_set(&mut accounts, admin, &set_id);
        accounts
            .get(admin)
            .and_then(|s| s.stack_sets.get(&set_id))
            .cloned()
            .ok_or_else(|| stack_set_not_found(name))
    }

    /// Reject a request planned against a snapshot that another operation has
    /// since changed.
    fn check_not_stale(current: &StackSet, snapshot: &StackSet) -> Result<(), AwsServiceError> {
        if current.operations.len() != snapshot.operations.len() {
            return Err(aws_err(
                StatusCode::CONFLICT,
                "StaleRequestException",
                format!(
                    "Another operation has been performed on StackSet {} since this request was made",
                    current.stack_set_id
                ),
            ));
        }
        Ok(())
    }

    fn delete_stack_set(
        &self,
        req: &AwsRequest,
        params: &BTreeMap<String, String>,
    ) -> Result<AwsResponse, AwsServiceError> {
        let name = required(params, "StackSetName")?;
        let admin = self.stack_set_admin_account(req, params)?;
        let mut accounts = self.state.write();
        // DeleteStackSet declares no not-found error; deleting a stack set that
        // does not exist is a no-op.
        let Some(set_id) = accounts
            .get(&admin)
            .and_then(|s| active_key(s, &name, Scope::of(params)))
        else {
            return Ok(xml_response_no_result("DeleteStackSet", &req.request_id));
        };
        Self::refresh_stack_set(&mut accounts, &admin, &set_id);
        let state = accounts.get_or_create(&admin);
        let Some(set) = state.stack_sets.get_mut(&set_id) else {
            return Ok(xml_response_no_result("DeleteStackSet", &req.request_id));
        };
        if set
            .operations
            .iter()
            .any(|o| matches!(o.status.as_str(), "RUNNING" | "STOPPING"))
        {
            return Err(aws_err(
                StatusCode::CONFLICT,
                "OperationInProgressException",
                format!("Another Operation on StackSet {set_id} is in progress"),
            ));
        }
        if !set.instances.is_empty() {
            return Err(aws_err(
                StatusCode::CONFLICT,
                "StackSetNotEmptyException",
                format!("StackSet {name} is not empty"),
            ));
        }
        set.status = "DELETED".to_string();
        Ok(xml_response_no_result("DeleteStackSet", &req.request_id))
    }

    // ── Stack instances ──

    /// Resolve the targets of CreateStackInstances from the request.
    fn resolve_new_targets(
        &self,
        set: &StackSet,
        admin: &str,
        accounts: &[String],
        deployment_targets: Option<&DeploymentTargets>,
        regions: &[String],
    ) -> Result<Vec<Target>, AwsServiceError> {
        if regions.is_empty() {
            return Err(validation("Regions is required"));
        }
        if !accounts.is_empty() && deployment_targets.is_some() {
            return Err(validation(
                "Only one of Accounts or DeploymentTargets can be specified",
            ));
        }
        let mut resolved: Vec<(String, Option<String>, bool)> = Vec::new();
        if set.permission_model == "SERVICE_MANAGED" {
            if !accounts.is_empty() {
                return Err(validation(
                    "StackSets with SERVICE_MANAGED permission model can only have OrganizationalUnit as target",
                ));
            }
            let dt = deployment_targets
                .filter(|t| !t.organizational_unit_ids.is_empty())
                .ok_or_else(|| {
                    validation("DeploymentTargets.OrganizationalUnitIds is required for SERVICE_MANAGED stack sets")
                })?;
            let filter_accounts = self.target_accounts_list(admin, dt)?;
            let filter = self.account_filter_type(dt, &filter_accounts)?;
            let orgs = self.deps.organizations.read();
            let org = orgs
                .as_ref()
                .ok_or_else(|| validation("AWS Organizations is not enabled for this account"))?;
            let mut seen = BTreeSet::new();
            for ou in &dt.organizational_unit_ids {
                if *ou != org.root_id && !org.ous.contains_key(ou) {
                    return Err(validation(format!(
                        "OrganizationalUnit {ou} does not exist"
                    )));
                }
                for (account, suspended) in accounts_under(org, ou) {
                    let listed = filter_accounts.contains(&account);
                    let keep = match filter.as_str() {
                        "INTERSECTION" => listed,
                        "DIFFERENCE" => !listed,
                        _ => true,
                    };
                    if keep && seen.insert(account.clone()) {
                        resolved.push((account, Some(ou.clone()), suspended));
                    }
                }
            }
            if filter == "UNION" {
                for account in filter_accounts {
                    if account != org.management_account_id && seen.insert(account.clone()) {
                        let suspended = org
                            .accounts
                            .get(&account)
                            .is_some_and(|a| a.status != "ACTIVE");
                        resolved.push((account, None, suspended));
                    }
                }
            }
        } else {
            let list = match deployment_targets {
                Some(dt) => {
                    if !dt.organizational_unit_ids.is_empty() {
                        return Err(validation(
                            "OrganizationalUnitIds are only supported for stack sets with SERVICE_MANAGED permission model",
                        ));
                    }
                    self.target_accounts_list(admin, dt)?
                }
                None => accounts.to_vec(),
            };
            if list.is_empty() {
                return Err(validation(
                    "Accounts or DeploymentTargets must be specified",
                ));
            }
            if let Some(bad) = list.iter().find(|a| !is_account_id(a)) {
                return Err(validation(format!(
                    "Account {bad} is not a valid AWS account id"
                )));
            }
            let mut seen = BTreeSet::new();
            for account in list {
                if seen.insert(account.clone()) {
                    resolved.push((account, None, false));
                }
            }
        }
        let mut targets = Vec::new();
        for region in regions {
            for (account, ou, suspended) in &resolved {
                targets.push(Target {
                    account: account.clone(),
                    region: region.clone(),
                    ou: ou.clone(),
                    suspended: *suspended,
                });
            }
        }
        Ok(targets)
    }

    /// `DeploymentTargets.Accounts` plus the accounts listed in
    /// `DeploymentTargets.AccountsUrl`, validated.
    fn target_accounts_list(
        &self,
        admin: &str,
        dt: &DeploymentTargets,
    ) -> Result<Vec<String>, AwsServiceError> {
        let mut list = dt.accounts.clone();
        if let Some(url) = &dt.accounts_url {
            if !looks_like_url(url) {
                return Err(validation(format!(
                    "AccountsUrl {url} is not a valid S3 URL"
                )));
            }
            let body = self
                .resolve_template_url(admin, url)
                .map_err(|_| validation(format!("Unable to read the accounts file at {url}")))?;
            list.extend(
                body.split([',', '\n', '\r'])
                    .map(str::trim)
                    .filter(|a| !a.is_empty())
                    .map(str::to_string),
            );
        }
        if let Some(bad) = list.iter().find(|a| !is_account_id(a)) {
            return Err(validation(format!(
                "Account {bad} is not a valid AWS account id"
            )));
        }
        Ok(list)
    }

    fn account_filter_type(
        &self,
        dt: &DeploymentTargets,
        filter_accounts: &[String],
    ) -> Result<String, AwsServiceError> {
        let filter = match (&dt.account_filter_type, filter_accounts.is_empty()) {
            (Some(f), _) => f.clone(),
            // Accounts next to OUs without an explicit filter narrow the OUs.
            (None, false) => "INTERSECTION".to_string(),
            (None, true) => "NONE".to_string(),
        };
        match filter.as_str() {
            "NONE" if !filter_accounts.is_empty() => Err(validation(
                "AccountFilterType NONE cannot be used together with Accounts",
            )),
            "INTERSECTION" | "DIFFERENCE" | "UNION" if filter_accounts.is_empty() => {
                Err(validation(format!(
                    "Accounts must be specified when AccountFilterType is {filter}"
                )))
            }
            "NONE" | "INTERSECTION" | "DIFFERENCE" | "UNION" => Ok(filter),
            other => Err(validation(format!("Invalid AccountFilterType {other}"))),
        }
    }

    /// Resolve targets that must name existing instances (UpdateStackInstances,
    /// DeleteStackInstances, a partial UpdateStackSet).
    fn existing_instance_targets(
        &self,
        set: &StackSet,
        admin: &str,
        accounts: &[String],
        deployment_targets: Option<&DeploymentTargets>,
        regions: &[String],
        must_exist: bool,
    ) -> Result<Vec<Target>, AwsServiceError> {
        if regions.is_empty() {
            return Err(validation("Regions is required"));
        }
        if !accounts.is_empty() && deployment_targets.is_some() {
            return Err(validation(
                "Only one of Accounts or DeploymentTargets can be specified",
            ));
        }
        let mut targets = Vec::new();
        let mut push = |instance: &StackInstance| {
            if !targets
                .iter()
                .any(|t: &Target| t.account == instance.account && t.region == instance.region)
            {
                targets.push(Target {
                    account: instance.account.clone(),
                    region: instance.region.clone(),
                    ou: instance.organizational_unit_id.clone(),
                    suspended: false,
                });
            }
        };
        if set.permission_model == "SERVICE_MANAGED" {
            if !accounts.is_empty() {
                return Err(validation(
                    "StackSets with SERVICE_MANAGED permission model can only have OrganizationalUnit as target",
                ));
            }
            let dt = deployment_targets
                .filter(|t| !t.organizational_unit_ids.is_empty())
                .ok_or_else(|| {
                    validation("DeploymentTargets.OrganizationalUnitIds is required for SERVICE_MANAGED stack sets")
                })?;
            let filter_accounts = self.target_accounts_list(admin, dt)?;
            let filter = self.account_filter_type(dt, &filter_accounts)?;
            // An OU covers every OU nested below it, so an instance deployed
            // through a child OU is reached through its parent or the root,
            // and one deployed through a parent is reached through a child.
            // The OU recorded on the instance still counts, for an account
            // that has since moved out.
            let accounts_in_ous: BTreeSet<String> = self
                .deps
                .organizations
                .read()
                .as_ref()
                .map(|org| {
                    dt.organizational_unit_ids
                        .iter()
                        .flat_map(|ou| accounts_under(org, ou))
                        .map(|(account, _)| account)
                        .collect()
                })
                .unwrap_or_default();
            for region in regions {
                for instance in set.instances.iter().filter(|i| &i.region == region) {
                    let in_ou = accounts_in_ous.contains(&instance.account)
                        || instance
                            .organizational_unit_id
                            .as_ref()
                            .is_some_and(|ou| dt.organizational_unit_ids.contains(ou));
                    let listed = filter_accounts.contains(&instance.account);
                    let keep = match filter.as_str() {
                        "INTERSECTION" => in_ou && listed,
                        "DIFFERENCE" => in_ou && !listed,
                        "UNION" => in_ou || listed,
                        _ => in_ou,
                    };
                    if keep {
                        push(instance);
                    }
                }
            }
        } else {
            let list = match deployment_targets {
                Some(dt) => {
                    if !dt.organizational_unit_ids.is_empty() {
                        return Err(validation(
                            "OrganizationalUnitIds are only supported for stack sets with SERVICE_MANAGED permission model",
                        ));
                    }
                    self.target_accounts_list(admin, dt)?
                }
                None => accounts.to_vec(),
            };
            if list.is_empty() {
                return Err(validation(
                    "Accounts or DeploymentTargets must be specified",
                ));
            }
            if let Some(bad) = list.iter().find(|a| !is_account_id(a)) {
                return Err(validation(format!(
                    "Account {bad} is not a valid AWS account id"
                )));
            }
            for region in regions {
                for account in &list {
                    match set
                        .instances
                        .iter()
                        .find(|i| &i.account == account && &i.region == region)
                    {
                        Some(instance) => push(instance),
                        None if must_exist => {
                            return Err(instance_not_found(&set.name, account, region))
                        }
                        None => {}
                    }
                }
            }
        }
        Ok(targets)
    }

    /// Validate that overrides only touch parameters the template declares.
    fn check_overrides_declared(
        set: &StackSet,
        keys: impl Iterator<Item = String>,
    ) -> Result<(), AwsServiceError> {
        let Ok(template) = fakecloud_core::cfn_template::parse_template_body(&set.template_body)
        else {
            return Ok(());
        };
        let declared: BTreeSet<String> = template
            .get("Parameters")
            .and_then(Value::as_object)
            .map(|o| o.keys().cloned().collect())
            .unwrap_or_default();
        for key in keys {
            if !declared.contains(&key) && !set.parameters.contains_key(&key) {
                return Err(validation(format!(
                    "Parameter {key} is not declared in the stack set template"
                )));
            }
        }
        Ok(())
    }

    /// Record a new instance operation, re-validating under the lock the
    /// snapshot its targets were resolved against.
    #[allow(clippy::too_many_arguments)]
    fn start_instance_operation(
        &self,
        admin: &str,
        snapshot: &StackSet,
        op_id: &str,
        action: &str,
        preferences: OperationPreferences,
        deployment_targets: Option<DeploymentTargets>,
        retain_stacks: Option<bool>,
    ) -> Result<DeploySpec, AwsServiceError> {
        let mut accounts = self.state.write();
        Self::refresh_stack_set(&mut accounts, admin, &snapshot.stack_set_id);
        let set = accounts
            .get_mut(admin)
            .and_then(|s| s.stack_sets.get_mut(&snapshot.stack_set_id))
            .filter(|s| s.status == "ACTIVE")
            .ok_or_else(|| stack_set_not_found(&snapshot.name))?;
        Self::check_not_stale(set, snapshot)?;
        Self::check_can_start_operation(set, op_id)?;
        let op = Self::new_operation(
            set,
            op_id,
            action,
            preferences,
            deployment_targets,
            retain_stacks,
        );
        set.operations.push(op);
        Ok(DeploySpec::of(set))
    }

    async fn create_stack_instances(
        &self,
        req: &AwsRequest,
        params: &BTreeMap<String, String>,
    ) -> Result<AwsResponse, AwsServiceError> {
        let name = required(params, "StackSetName")?;
        let admin = self.stack_set_admin_account(req, params)?;
        let regions = member_list(params, "Regions");
        let accounts = member_list(params, "Accounts");
        let deployment_targets = parse_deployment_targets(params);
        let preferences = parse_preferences(params)?;
        let op_id = params
            .get("OperationId")
            .cloned()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let overrides = resolve_parameters(
            &parameter_list(params, "ParameterOverrides"),
            &BTreeMap::new(),
        )?;

        // Target resolution reads Organizations (and possibly S3) state, so it
        // runs against a snapshot of the stack set before the operation is
        // recorded under the CloudFormation lock.
        let snapshot = self.active_snapshot(&admin, &name, Scope::of(params))?;
        Self::check_overrides_declared(&snapshot, overrides.keys().cloned())?;
        let targets = self.resolve_new_targets(
            &snapshot,
            &admin,
            &accounts,
            deployment_targets.as_ref(),
            &regions,
        )?;
        let targets = order_targets(targets, &regions, &preferences);
        let record = Some(targets_record(&accounts, deployment_targets.as_ref()));
        let spec = self.start_instance_operation(
            &admin,
            &snapshot,
            &op_id,
            "CREATE",
            preferences,
            record,
            None,
        )?;
        let set_id = snapshot.stack_set_id.clone();
        self.run_operation(
            req,
            &admin,
            &set_id,
            &op_id,
            &spec,
            targets,
            TargetAction::Create { overrides },
        )
        .await;
        Ok(xml_response(
            "CreateStackInstances",
            el("OperationId", &op_id),
            &req.request_id,
        ))
    }

    async fn update_stack_instances(
        &self,
        req: &AwsRequest,
        params: &BTreeMap<String, String>,
    ) -> Result<AwsResponse, AwsServiceError> {
        let name = required(params, "StackSetName")?;
        let admin = self.stack_set_admin_account(req, params)?;
        let regions = member_list(params, "Regions");
        let accounts = member_list(params, "Accounts");
        let deployment_targets = parse_deployment_targets(params);
        let preferences = parse_preferences(params)?;
        let op_id = params
            .get("OperationId")
            .cloned()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let overrides = list_present(params, "ParameterOverrides").then(|| {
            parameter_list(params, "ParameterOverrides")
                .into_iter()
                .map(|e| match (e.value, e.use_previous) {
                    (Some(v), false) => Ok(OverrideSpec::Value(e.key, v)),
                    (None, true) => Ok(OverrideSpec::UsePrevious(e.key)),
                    (Some(_), true) => Err(validation(format!(
                        "Invalid input for parameter key {}. Cannot specify usePreviousValue as true and a parameter value at the same time",
                        e.key
                    ))),
                    (None, false) => Err(validation(format!(
                        "Invalid input for parameter key {}. Need to specify either usePreviousValue as true or a value for the parameter",
                        e.key
                    ))),
                })
                .collect::<Result<Vec<_>, _>>()
        });
        let overrides = overrides.transpose()?;

        let snapshot = self.active_snapshot(&admin, &name, Scope::of(params))?;
        if let Some(specs) = &overrides {
            Self::check_overrides_declared(
                &snapshot,
                specs.iter().map(|s| match s {
                    OverrideSpec::Value(k, _) | OverrideSpec::UsePrevious(k) => k.clone(),
                }),
            )?;
        }
        let targets = self.existing_instance_targets(
            &snapshot,
            &admin,
            &accounts,
            deployment_targets.as_ref(),
            &regions,
            true,
        )?;
        let targets = order_targets(targets, &regions, &preferences);
        let record = Some(targets_record(&accounts, deployment_targets.as_ref()));
        let spec = self.start_instance_operation(
            &admin,
            &snapshot,
            &op_id,
            "UPDATE",
            preferences,
            record,
            None,
        )?;
        let set_id = snapshot.stack_set_id.clone();
        self.run_operation(
            req,
            &admin,
            &set_id,
            &op_id,
            &spec,
            targets,
            TargetAction::Update { overrides },
        )
        .await;
        Ok(xml_response(
            "UpdateStackInstances",
            el("OperationId", &op_id),
            &req.request_id,
        ))
    }

    async fn delete_stack_instances(
        &self,
        req: &AwsRequest,
        params: &BTreeMap<String, String>,
    ) -> Result<AwsResponse, AwsServiceError> {
        let name = required(params, "StackSetName")?;
        let admin = self.stack_set_admin_account(req, params)?;
        let retain_stacks = parse_bool(params, "RetainStacks")?
            .ok_or_else(|| validation("RetainStacks is required"))?;
        let regions = member_list(params, "Regions");
        let accounts = member_list(params, "Accounts");
        let deployment_targets = parse_deployment_targets(params);
        let preferences = parse_preferences(params)?;
        let op_id = params
            .get("OperationId")
            .cloned()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

        let snapshot = self.active_snapshot(&admin, &name, Scope::of(params))?;
        let targets = self.existing_instance_targets(
            &snapshot,
            &admin,
            &accounts,
            deployment_targets.as_ref(),
            &regions,
            false,
        )?;
        let targets = order_targets(targets, &regions, &preferences);
        let record = Some(targets_record(&accounts, deployment_targets.as_ref()));
        let spec = self.start_instance_operation(
            &admin,
            &snapshot,
            &op_id,
            "DELETE",
            preferences,
            record,
            Some(retain_stacks),
        )?;
        let set_id = snapshot.stack_set_id.clone();
        self.run_operation(
            req,
            &admin,
            &set_id,
            &op_id,
            &spec,
            targets,
            TargetAction::Delete { retain_stacks },
        )
        .await;
        Ok(xml_response(
            "DeleteStackInstances",
            el("OperationId", &op_id),
            &req.request_id,
        ))
    }

    /// Run an operation's targets in order, recording each outcome as it
    /// lands, honoring the failure tolerance and StopStackSetOperation.
    #[allow(clippy::too_many_arguments)]
    async fn run_operation(
        &self,
        req: &AwsRequest,
        admin: &str,
        set_id: &str,
        op_id: &str,
        spec: &DeploySpec,
        targets: Vec<Target>,
        action: TargetAction,
    ) {
        // Seed a PENDING result per target so the operation reports every
        // target it will act on from the start.
        {
            let mut accounts = self.state.write();
            if let Some(op) = accounts
                .get_mut(admin)
                .and_then(|s| s.stack_sets.get_mut(set_id))
                .and_then(|set| set.operations.iter_mut().find(|o| o.operation_id == op_id))
            {
                op.results = targets
                    .iter()
                    .map(|t| OperationResult {
                        account: t.account.clone(),
                        region: t.region.clone(),
                        status: "PENDING".to_string(),
                        status_reason: None,
                        organizational_unit_id: t.ou.clone(),
                        account_gate_status: None,
                        account_gate_reason: None,
                    })
                    .collect();
            }
        }

        let prefs = {
            let accounts = self.state.read();
            accounts
                .get(admin)
                .and_then(|s| s.stack_sets.get(set_id))
                .and_then(|set| set.operations.iter().find(|o| o.operation_id == op_id))
                .map(|o| o.preferences.clone())
                .unwrap_or_default()
        };
        let mut region_sizes: BTreeMap<String, usize> = BTreeMap::new();
        for t in &targets {
            *region_sizes.entry(t.region.clone()).or_default() += 1;
        }
        let mut region_failures: BTreeMap<String, usize> = BTreeMap::new();
        let mut abort: Option<&'static str> = None;

        for target in targets {
            // Claiming the target marks it RUNNING under the lock, so a
            // StopStackSetOperation that lands while it deploys cannot settle
            // the operation as STOPPED underneath it.
            if abort.is_none() && !self.claim_target(admin, set_id, op_id, &target) {
                abort = Some(OPERATION_STOPPED);
            }
            let mut gate = None;
            let mut stack_id = None;
            let mut overrides = None;
            let outcome = if let Some(reason) = abort {
                Outcome::Cancelled(reason.to_string())
            } else if target.suspended {
                Outcome::SkippedSuspended
            } else {
                let g = self.account_gate(&target.account, &target.region).await;
                let passed = g.status != "FAILED";
                let gate_reason = g.reason.clone();
                gate = Some(g);
                if passed {
                    let (outcome, id, applied) = self
                        .apply_target(req, admin, set_id, spec, &target, &action)
                        .await;
                    stack_id = id;
                    overrides = applied;
                    outcome
                } else {
                    Outcome::Failed(
                        gate_reason.unwrap_or_else(|| "Account gate check failed".to_string()),
                    )
                }
            };
            if matches!(outcome, Outcome::Failed(_)) {
                let failures = region_failures.entry(target.region.clone()).or_default();
                *failures += 1;
                let size = region_sizes.get(&target.region).copied().unwrap_or(0);
                if *failures > region_tolerance(&prefs, size) {
                    abort = Some(TOLERANCE_EXCEEDED);
                }
            }
            self.record_outcome(
                admin, set_id, op_id, &target, &action, &outcome, gate, stack_id, overrides,
            );
        }

        let mut accounts = self.state.write();
        if let Some(op) = accounts
            .get_mut(admin)
            .and_then(|s| s.stack_sets.get_mut(set_id))
            .and_then(|set| set.operations.iter_mut().find(|o| o.operation_id == op_id))
        {
            settle_operation(op);
        }
    }

    /// Mark a target's result RUNNING before it deploys. Returns false when
    /// the operation has been stopped, in which case the target must not run.
    fn claim_target(&self, admin: &str, set_id: &str, op_id: &str, target: &Target) -> bool {
        let mut accounts = self.state.write();
        let Some(op) = accounts
            .get_mut(admin)
            .and_then(|s| s.stack_sets.get_mut(set_id))
            .and_then(|set| set.operations.iter_mut().find(|o| o.operation_id == op_id))
        else {
            return false;
        };
        if op.status != "RUNNING" {
            return false;
        }
        if let Some(result) = op
            .results
            .iter_mut()
            .find(|r| r.account == target.account && r.region == target.region)
        {
            result.status = "RUNNING".to_string();
        }
        true
    }

    /// Run the account's `AWSCloudFormationStackSetAccountGate` Lambda, if it
    /// has one. A deployment proceeds only when the function answers
    /// `SUCCEEDED`; an account without the function is not gated.
    async fn account_gate(&self, account: &str, region: &str) -> GateResult {
        let arn = format!("arn:aws:lambda:{region}:{account}:function:{ACCOUNT_GATE_FUNCTION}");
        let exists = self
            .deps
            .lambda
            .read()
            .get(account)
            .is_some_and(|s| s.functions.contains_key(ACCOUNT_GATE_FUNCTION));
        if !exists {
            return GateResult {
                status: "SKIPPED",
                reason: Some(format!("Function not found: {arn}")),
            };
        }
        match self.deps.delivery.invoke_lambda(&arn, "{}").await {
            None => GateResult {
                status: "SKIPPED",
                reason: Some("Lambda invocation is not available".to_string()),
            },
            Some(Err(e)) => GateResult {
                status: "FAILED",
                reason: Some(format!("Account gate function invocation failed: {e}")),
            },
            Some(Ok(bytes)) => {
                let status = serde_json::from_slice::<Value>(&bytes)
                    .ok()
                    .and_then(|v| v.get("Status").and_then(Value::as_str).map(str::to_string));
                match status.as_deref() {
                    Some("SUCCEEDED") => GateResult {
                        status: "SUCCEEDED",
                        reason: None,
                    },
                    other => GateResult {
                        status: "FAILED",
                        reason: Some(format!(
                            "Account gate function returned {}",
                            other.unwrap_or("an invalid response")
                        )),
                    },
                }
            }
        }
    }

    fn instance_stack(&self, admin: &str, set_id: &str, target: &Target) -> Option<StackInstance> {
        self.state
            .read()
            .get(admin)
            .and_then(|s| s.stack_sets.get(set_id))
            .and_then(|set| {
                set.instances
                    .iter()
                    .find(|i| i.account == target.account && i.region == target.region)
                    .cloned()
            })
    }

    fn stack_status(
        &self,
        account: &str,
        stack_id_or_name: &str,
    ) -> Option<(String, String, Option<String>)> {
        self.state.read().get(account).and_then(|s| {
            s.stacks
                .values()
                .filter(|st| st.stack_id == stack_id_or_name || st.name == stack_id_or_name)
                .max_by_key(|st| st.created_at)
                .map(|st| {
                    (
                        st.stack_id.clone(),
                        st.status.clone(),
                        st.status_reason.clone(),
                    )
                })
        })
    }

    /// Deploy one target. Returns its outcome, the instance's stack id, and
    /// the overrides the instance now carries.
    async fn apply_target(
        &self,
        req: &AwsRequest,
        admin: &str,
        set_id: &str,
        spec: &DeploySpec,
        target: &Target,
        action: &TargetAction,
    ) -> (Outcome, Option<String>, Option<BTreeMap<String, String>>) {
        let existing = self.instance_stack(admin, set_id, target);
        let live_stack = existing
            .as_ref()
            .and_then(|i| i.stack_id.as_deref())
            .and_then(|id| self.stack_status(&target.account, id))
            .filter(|(_, status, _)| status != "DELETE_COMPLETE");

        match action {
            TargetAction::Delete { retain_stacks } => {
                let Some((stack_id, _, _)) = live_stack else {
                    return (Outcome::Succeeded, None, None);
                };
                if *retain_stacks {
                    return (Outcome::Succeeded, Some(stack_id), None);
                }
                let request = synthetic_request(
                    &target.account,
                    &target.region,
                    "DeleteStack",
                    &req.request_id,
                    vec![("StackName".to_string(), stack_id.clone())],
                );
                if let Err(e) = self.delete_stack(&request).await {
                    return (Outcome::Failed(e.message()), Some(stack_id), None);
                }
                let outcome = match self.stack_status(&target.account, &stack_id) {
                    Some((_, status, reason)) => {
                        stack_outcome("DELETE", &status, reason.as_deref())
                    }
                    None => Outcome::Succeeded,
                };
                (outcome, Some(stack_id), None)
            }
            TargetAction::Create { overrides } => match live_stack {
                Some((stack_id, _, _)) => {
                    let (outcome, id) = self
                        .update_instance_stack(req, spec, target, &stack_id, overrides)
                        .await;
                    (outcome, id, Some(overrides.clone()))
                }
                None => {
                    let stack_name = format!(
                        "StackSet-{}-{}",
                        spec.name.replace(':', "-"),
                        uuid::Uuid::new_v4()
                    );
                    let mut stack_params = spec.stack_params(overrides);
                    stack_params.push(("StackName".to_string(), stack_name.clone()));
                    let request = synthetic_request(
                        &target.account,
                        &target.region,
                        "CreateStack",
                        &req.request_id,
                        stack_params,
                    );
                    if let Err(e) = self.create_stack(&request).await {
                        return (Outcome::Failed(e.message()), None, Some(overrides.clone()));
                    }
                    match self.stack_status(&target.account, &stack_name) {
                        Some((stack_id, status, reason)) => (
                            stack_outcome("CREATE", &status, reason.as_deref()),
                            Some(stack_id),
                            Some(overrides.clone()),
                        ),
                        None => (
                            Outcome::Failed(format!("Stack {stack_name} was not created")),
                            None,
                            Some(overrides.clone()),
                        ),
                    }
                }
            },
            TargetAction::Update { overrides } => {
                let previous = existing
                    .as_ref()
                    .map(|i| i.parameter_overrides.clone())
                    .unwrap_or_default();
                let resolved = match overrides {
                    None => previous,
                    Some(specs) => specs
                        .iter()
                        .filter_map(|s| match s {
                            OverrideSpec::Value(k, v) => Some((k.clone(), v.clone())),
                            OverrideSpec::UsePrevious(k) => {
                                previous.get(k).map(|v| (k.clone(), v.clone()))
                            }
                        })
                        .collect(),
                };
                let Some((stack_id, _, _)) = live_stack else {
                    let missing = existing.and_then(|i| i.stack_id).unwrap_or_default();
                    return (
                        Outcome::Failed(format!("Stack [{missing}] does not exist")),
                        None,
                        Some(resolved),
                    );
                };
                let (outcome, id) = self
                    .update_instance_stack(req, spec, target, &stack_id, &resolved)
                    .await;
                (outcome, id, Some(resolved))
            }
        }
    }

    async fn update_instance_stack(
        &self,
        req: &AwsRequest,
        spec: &DeploySpec,
        target: &Target,
        stack_id: &str,
        overrides: &BTreeMap<String, String>,
    ) -> (Outcome, Option<String>) {
        let mut stack_params = spec.stack_params(overrides);
        stack_params.push(("StackName".to_string(), stack_id.to_string()));
        let request = synthetic_request(
            &target.account,
            &target.region,
            "UpdateStack",
            &req.request_id,
            stack_params,
        );
        if let Err(e) = self.update_stack(&request).await {
            return (Outcome::Failed(e.message()), Some(stack_id.to_string()));
        }
        let outcome = match self.stack_status(&target.account, stack_id) {
            Some((_, status, reason)) => stack_outcome("UPDATE", &status, reason.as_deref()),
            None => Outcome::Failed(format!("Stack [{stack_id}] does not exist")),
        };
        (outcome, Some(stack_id.to_string()))
    }

    #[allow(clippy::too_many_arguments)]
    fn record_outcome(
        &self,
        admin: &str,
        set_id: &str,
        op_id: &str,
        target: &Target,
        action: &TargetAction,
        outcome: &Outcome,
        gate: Option<GateResult>,
        stack_id: Option<String>,
        overrides: Option<BTreeMap<String, String>>,
    ) {
        let mut accounts = self.state.write();
        let Some(set) = accounts
            .get_mut(admin)
            .and_then(|s| s.stack_sets.get_mut(set_id))
        else {
            return;
        };
        if let Some(result) = set
            .operations
            .iter_mut()
            .find(|o| o.operation_id == op_id)
            .and_then(|op| {
                op.results
                    .iter_mut()
                    .find(|r| r.account == target.account && r.region == target.region)
            })
        {
            result.status = result_status(outcome).to_string();
            result.status_reason = outcome_reason(outcome);
            if let Some(gate) = gate {
                result.account_gate_status = Some(gate.status.to_string());
                result.account_gate_reason = gate.reason;
            }
        }

        let position = set
            .instances
            .iter()
            .position(|i| i.account == target.account && i.region == target.region);
        match (action, position) {
            (TargetAction::Delete { .. }, Some(idx)) => {
                if *outcome == Outcome::Succeeded {
                    set.instances.remove(idx);
                } else if !matches!(outcome, Outcome::Cancelled(_)) {
                    let instance = &mut set.instances[idx];
                    apply_to_instance(instance, outcome);
                    instance.last_operation_id = Some(op_id.to_string());
                }
            }
            (TargetAction::Delete { .. }, None) => {}
            (_, position) => {
                let idx = match position {
                    Some(idx) => idx,
                    None => {
                        set.instances.push(StackInstance {
                            account: target.account.clone(),
                            region: target.region.clone(),
                            stack_id: None,
                            status: "OUTDATED".to_string(),
                            detailed_status: "PENDING".to_string(),
                            status_reason: None,
                            parameter_overrides: BTreeMap::new(),
                            organizational_unit_id: target.ou.clone(),
                            drift_status: "NOT_CHECKED".to_string(),
                            last_drift_check_timestamp: None,
                            last_operation_id: None,
                        });
                        set.instances.len() - 1
                    }
                };
                let instance = &mut set.instances[idx];
                apply_to_instance(instance, outcome);
                instance.last_operation_id = Some(op_id.to_string());
                if stack_id.is_some() {
                    instance.stack_id = stack_id;
                }
                if let Some(overrides) = overrides {
                    instance.parameter_overrides = overrides;
                }
                if target.ou.is_some() {
                    instance.organizational_unit_id = target.ou.clone();
                }
            }
        }
    }

    fn describe_stack_instance(
        &self,
        req: &AwsRequest,
        params: &BTreeMap<String, String>,
    ) -> Result<AwsResponse, AwsServiceError> {
        let name = required(params, "StackSetName")?;
        let account = required(params, "StackInstanceAccount")?;
        let region = required(params, "StackInstanceRegion")?;
        let admin = self.stack_set_admin_account(req, params)?;
        let set = self.read_stack_set(&admin, &name, Scope::of(params))?;
        let instance = set
            .instances
            .iter()
            .find(|i| i.account == account && i.region == region)
            .ok_or_else(|| instance_not_found(&name, &account, &region))?;
        let inner = format!(
            "<StackInstance>{}</StackInstance>",
            instance_fields(&set, instance, true)
        );
        Ok(xml_response(
            "DescribeStackInstance",
            inner,
            &req.request_id,
        ))
    }

    fn list_stack_instances(
        &self,
        req: &AwsRequest,
        params: &BTreeMap<String, String>,
    ) -> Result<AwsResponse, AwsServiceError> {
        let name = required(params, "StackSetName")?;
        let admin = self.stack_set_admin_account(req, params)?;
        let set = self.read_stack_set(&admin, &name, Scope::of(params))?;
        let mut filters: Vec<(String, String)> = Vec::new();
        for i in 1.. {
            let Some(filter_name) = params.get(&format!("Filters.member.{i}.Name")) else {
                break;
            };
            let value = params
                .get(&format!("Filters.member.{i}.Values"))
                .cloned()
                .unwrap_or_default();
            if !matches!(
                filter_name.as_str(),
                "DETAILED_STATUS" | "LAST_OPERATION_ID" | "DRIFT_STATUS"
            ) {
                return Err(validation(format!("Invalid filter name {filter_name}")));
            }
            filters.push((filter_name.clone(), value));
        }
        let account = params.get("StackInstanceAccount");
        let region = params.get("StackInstanceRegion");
        let matching: Vec<&StackInstance> = set
            .instances
            .iter()
            .filter(|i| account.is_none_or(|a| &i.account == a))
            .filter(|i| region.is_none_or(|r| &i.region == r))
            .filter(|i| {
                filters.iter().all(|(name, value)| match name.as_str() {
                    "DETAILED_STATUS" => &i.detailed_status == value,
                    "LAST_OPERATION_ID" => i.last_operation_id.as_ref() == Some(value),
                    _ => &i.drift_status == value,
                })
            })
            .collect();
        let (page, next) = paginate(matching, params)?;
        let inner = format!(
            "{}{}",
            list_el(
                "Summaries",
                page.iter().map(|i| instance_fields(&set, i, false))
            ),
            next_token_el(next)
        );
        Ok(xml_response("ListStackInstances", inner, &req.request_id))
    }

    // ── Operations ──

    fn describe_stack_set_operation(
        &self,
        req: &AwsRequest,
        params: &BTreeMap<String, String>,
    ) -> Result<AwsResponse, AwsServiceError> {
        let name = required(params, "StackSetName")?;
        let op_id = required(params, "OperationId")?;
        let admin = self.stack_set_admin_account(req, params)?;
        let set = self.read_stack_set(&admin, &name, Scope::of(params))?;
        let op = set
            .operations
            .iter()
            .find(|o| o.operation_id == op_id)
            .ok_or_else(|| operation_not_found(&op_id))?;
        Ok(xml_response(
            "DescribeStackSetOperation",
            operation_el(&set, op),
            &req.request_id,
        ))
    }

    fn list_stack_set_operations(
        &self,
        req: &AwsRequest,
        params: &BTreeMap<String, String>,
    ) -> Result<AwsResponse, AwsServiceError> {
        let name = required(params, "StackSetName")?;
        let admin = self.stack_set_admin_account(req, params)?;
        let set = self.read_stack_set(&admin, &name, Scope::of(params))?;
        // Most recent first.
        let ops: Vec<&StackSetOperation> = set.operations.iter().rev().collect();
        let (page, next) = paginate(ops, params)?;
        let inner = format!(
            "{}{}",
            list_el("Summaries", page.into_iter().map(operation_summary_el)),
            next_token_el(next)
        );
        Ok(xml_response(
            "ListStackSetOperations",
            inner,
            &req.request_id,
        ))
    }

    fn list_stack_set_operation_results(
        &self,
        req: &AwsRequest,
        params: &BTreeMap<String, String>,
    ) -> Result<AwsResponse, AwsServiceError> {
        let name = required(params, "StackSetName")?;
        let op_id = required(params, "OperationId")?;
        let admin = self.stack_set_admin_account(req, params)?;
        let set = self.read_stack_set(&admin, &name, Scope::of(params))?;
        let op = set
            .operations
            .iter()
            .find(|o| o.operation_id == op_id)
            .ok_or_else(|| operation_not_found(&op_id))?;
        let mut wanted_status: Option<String> = None;
        for i in 1.. {
            let Some(filter_name) = params.get(&format!("Filters.member.{i}.Name")) else {
                break;
            };
            if filter_name != "OPERATION_RESULT_STATUS" {
                return Err(validation(format!("Invalid filter name {filter_name}")));
            }
            wanted_status = params.get(&format!("Filters.member.{i}.Values")).cloned();
        }
        let results: Vec<&OperationResult> = op
            .results
            .iter()
            .filter(|r| wanted_status.as_ref().is_none_or(|s| &r.status == s))
            .collect();
        let (page, next) = paginate(results, params)?;
        let inner = format!(
            "{}{}",
            list_el("Summaries", page.into_iter().map(operation_result_el)),
            next_token_el(next)
        );
        Ok(xml_response(
            "ListStackSetOperationResults",
            inner,
            &req.request_id,
        ))
    }

    fn stop_stack_set_operation(
        &self,
        req: &AwsRequest,
        params: &BTreeMap<String, String>,
    ) -> Result<AwsResponse, AwsServiceError> {
        let name = required(params, "StackSetName")?;
        let op_id = required(params, "OperationId")?;
        let admin = self.stack_set_admin_account(req, params)?;
        let mut accounts = self.state.write();
        let set_id = accounts
            .get(&admin)
            .and_then(|s| find_for_read(s, &name, Scope::of(params)))
            .map(|s| s.stack_set_id.clone())
            .ok_or_else(|| stack_set_not_found(&name))?;
        Self::refresh_stack_set(&mut accounts, &admin, &set_id);
        let op = accounts
            .get_mut(&admin)
            .and_then(|s| s.stack_sets.get_mut(&set_id))
            .and_then(|set| set.operations.iter_mut().find(|o| o.operation_id == op_id))
            .ok_or_else(|| operation_not_found(&op_id))?;
        if op.status != "RUNNING" {
            return Err(aws_err(
                StatusCode::BAD_REQUEST,
                "InvalidOperationException",
                format!(
                    "Operation {op_id} is in {} state and cannot be stopped",
                    op.status
                ),
            ));
        }
        // Targets not yet started are cancelled; ones already deploying run to
        // completion, and the operation settles as STOPPED once they do.
        op.status = "STOPPING".to_string();
        for result in &mut op.results {
            if result.status == "PENDING" {
                result.status = "CANCELLED".to_string();
                result.status_reason = Some(OPERATION_STOPPED.to_string());
            }
        }
        settle_operation(op);
        Ok(xml_response(
            "StopStackSetOperation",
            String::new(),
            &req.request_id,
        ))
    }

    // ── Import ──

    fn import_stacks_to_stack_set(
        &self,
        req: &AwsRequest,
        params: &BTreeMap<String, String>,
    ) -> Result<AwsResponse, AwsServiceError> {
        let name = required(params, "StackSetName")?;
        let admin = self.stack_set_admin_account(req, params)?;
        let preferences = parse_preferences(params)?;
        let op_id = params
            .get("OperationId")
            .cloned()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let ous = member_list(params, "OrganizationalUnitIds");
        let mut stack_ids = member_list(params, "StackIds");
        if let Some(url) = params.get("StackIdsUrl") {
            if !stack_ids.is_empty() {
                return Err(validation(
                    "Only one of StackIds or StackIdsUrl can be specified",
                ));
            }
            if !looks_like_url(url) {
                return Err(validation(format!(
                    "StackIdsUrl {url} is not a valid S3 URL"
                )));
            }
            let body = self
                .resolve_template_url(&admin, url)
                .map_err(|_| validation(format!("Unable to read the stack ids file at {url}")))?;
            stack_ids = body
                .split([',', '\n', '\r'])
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect();
        }
        if stack_ids.is_empty() {
            return Err(validation("StackIds or StackIdsUrl must be specified"));
        }
        if stack_ids.len() > MAX_IMPORT_STACKS {
            return Err(aws_err(
                StatusCode::BAD_REQUEST,
                "LimitExceededException",
                format!("A maximum of {MAX_IMPORT_STACKS} stacks can be imported in one operation"),
            ));
        }

        // Where each stack lives, and which OU its account sits in.
        let mut located = Vec::new();
        for stack_id in &stack_ids {
            let (account, region) = stack_arn_location(stack_id)
                .ok_or_else(|| validation(format!("Invalid stack id {stack_id}")))?;
            located.push((stack_id.clone(), account, region));
        }

        // Which targeted OU each account sits in, read before the
        // CloudFormation lock is taken.
        let mut ou_of_account: BTreeMap<String, String> = BTreeMap::new();
        if !ous.is_empty() {
            let orgs = self.deps.organizations.read();
            let org = orgs
                .as_ref()
                .ok_or_else(|| validation("AWS Organizations is not enabled for this account"))?;
            for ou in &ous {
                for (account, _) in accounts_under(org, ou) {
                    ou_of_account.entry(account).or_insert_with(|| ou.clone());
                }
            }
        }

        let mut accounts = self.state.write();
        let set_id = accounts
            .get(&admin)
            .and_then(|s| active_key(s, &name, Scope::of(params)))
            .ok_or_else(|| stack_set_not_found(&name))?;
        Self::refresh_stack_set(&mut accounts, &admin, &set_id);
        let set = accounts
            .get(&admin)
            .and_then(|s| s.stack_sets.get(&set_id))
            .cloned()
            .ok_or_else(|| stack_set_not_found(&name))?;
        Self::check_can_start_operation(&set, &op_id)?;
        let service_managed = set.permission_model == "SERVICE_MANAGED";
        if service_managed && ous.is_empty() {
            return Err(validation(
                "OrganizationalUnitIds is required when importing into a SERVICE_MANAGED stack set",
            ));
        }
        if !service_managed && !ous.is_empty() {
            return Err(validation(
                "OrganizationalUnitIds are only supported for stack sets with SERVICE_MANAGED permission model",
            ));
        }

        let set_template =
            fakecloud_core::cfn_template::parse_template_body(&set.template_body).ok();
        let mut op = Self::new_operation(&set, &op_id, "CREATE", preferences, None, None);
        let mut new_instances = Vec::new();
        for (stack_id, account, region) in located {
            let stack = accounts
                .get(&account)
                .and_then(|s| {
                    s.stacks
                        .values()
                        .find(|st| st.stack_id == stack_id && st.status != "DELETE_COMPLETE")
                })
                .cloned()
                .ok_or_else(|| {
                    aws_err(
                        StatusCode::NOT_FOUND,
                        "StackNotFoundException",
                        format!("Stack with id {stack_id} does not exist"),
                    )
                })?;
            let ou = if service_managed {
                let ou = ou_of_account.get(&account).cloned().ok_or_else(|| {
                    validation(format!(
                        "Account {account} of stack {stack_id} is not in the specified OrganizationalUnitIds"
                    ))
                })?;
                Some(ou)
            } else {
                None
            };
            let already_managed = accounts.iter().any(|(_, s)| {
                s.stack_sets.values().any(|other| {
                    other.status == "ACTIVE"
                        && other
                            .instances
                            .iter()
                            .any(|i| i.stack_id.as_deref() == Some(stack_id.as_str()))
                })
            });
            let duplicate = set
                .instances
                .iter()
                .chain(new_instances.iter())
                .any(|i: &StackInstance| i.account == account && i.region == region);
            let template_matches = match (
                &set_template,
                fakecloud_core::cfn_template::parse_template_body(&stack.template),
            ) {
                (Some(a), Ok(b)) => *a == b,
                _ => set.template_body.trim() == stack.template.trim(),
            };
            let (result_status, reason, instance_status) = if already_managed {
                (
                    "FAILED",
                    Some(format!(
                        "Stack {stack_id} is already managed by a stack set"
                    )),
                    None,
                )
            } else if duplicate {
                (
                    "FAILED",
                    Some(format!(
                        "Stack instance for account {account} and region {region} already exists"
                    )),
                    None,
                )
            } else if !template_matches {
                (
                    "FAILED",
                    Some("The stack's template does not match the stack set template".to_string()),
                    Some(("OUTDATED", "FAILED_IMPORT")),
                )
            } else {
                ("SUCCEEDED", None, Some(("CURRENT", "SUCCEEDED")))
            };
            op.results.push(OperationResult {
                account: account.clone(),
                region: region.clone(),
                status: result_status.to_string(),
                status_reason: reason.clone(),
                organizational_unit_id: ou.clone(),
                account_gate_status: None,
                account_gate_reason: None,
            });
            if let Some((status, detailed)) = instance_status {
                let overrides = stack
                    .parameters
                    .iter()
                    .filter(|(k, v)| !k.starts_with("AWS::") && set.parameters.get(*k) != Some(*v))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                new_instances.push(StackInstance {
                    account,
                    region,
                    stack_id: Some(stack_id),
                    status: status.to_string(),
                    detailed_status: detailed.to_string(),
                    status_reason: reason,
                    parameter_overrides: overrides,
                    organizational_unit_id: ou,
                    drift_status: "NOT_CHECKED".to_string(),
                    last_drift_check_timestamp: None,
                    last_operation_id: Some(op_id.clone()),
                });
            }
        }
        op.status = settled_status(&op).to_string();
        op.ended_at = Some(Utc::now());
        let set = accounts
            .get_or_create(&admin)
            .stack_sets
            .get_mut(&set_id)
            .ok_or_else(|| stack_set_not_found(&name))?;
        set.instances.extend(new_instances);
        set.operations.push(op);
        Ok(xml_response(
            "ImportStacksToStackSet",
            el("OperationId", &op_id),
            &req.request_id,
        ))
    }

    fn list_stack_set_auto_deployment_targets(
        &self,
        req: &AwsRequest,
        params: &BTreeMap<String, String>,
    ) -> Result<AwsResponse, AwsServiceError> {
        let name = required(params, "StackSetName")?;
        let admin = self.stack_set_admin_account(req, params)?;
        let set = self.read_stack_set(&admin, &name, Scope::of(params))?;
        let mut by_ou: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        if set.permission_model == "SERVICE_MANAGED" {
            for instance in &set.instances {
                if let Some(ou) = &instance.organizational_unit_id {
                    by_ou
                        .entry(ou.clone())
                        .or_default()
                        .insert(instance.region.clone());
                }
            }
        }
        let entries: Vec<(String, BTreeSet<String>)> = by_ou.into_iter().collect();
        let (page, next) = paginate(entries, params)?;
        let inner = format!(
            "{}{}",
            list_el(
                "Summaries",
                page.iter().map(|(ou, regions)| {
                    format!(
                        "{}{}",
                        el("OrganizationalUnitId", ou),
                        scalar_list_el("Regions", regions)
                    )
                })
            ),
            next_token_el(next)
        );
        Ok(xml_response(
            "ListStackSetAutoDeploymentTargets",
            inner,
            &req.request_id,
        ))
    }

    // ── Drift ──

    fn detect_stack_set_drift(
        &self,
        req: &AwsRequest,
        params: &BTreeMap<String, String>,
    ) -> Result<AwsResponse, AwsServiceError> {
        let name = required(params, "StackSetName")?;
        let admin = self.stack_set_admin_account(req, params)?;
        let preferences = parse_preferences(params)?;
        let op_id = params
            .get("OperationId")
            .cloned()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

        let set = {
            let mut accounts = self.state.write();
            let set_id = accounts
                .get(&admin)
                .and_then(|s| active_key(s, &name, Scope::of(params)))
                .ok_or_else(|| stack_set_not_found(&name))?;
            Self::refresh_stack_set(&mut accounts, &admin, &set_id);
            let set = accounts
                .get(&admin)
                .and_then(|s| s.stack_sets.get(&set_id))
                .cloned()
                .ok_or_else(|| stack_set_not_found(&name))?;
            Self::check_can_start_operation(&set, &op_id)?;
            set
        };

        // Check every instance's stack against the live backing resources.
        let now = Utc::now();
        let mut op = Self::new_operation(&set, &op_id, "DETECT_DRIFT", preferences, None, None);
        let mut instance_drift: Vec<(String, String, String)> = Vec::new();
        for instance in &set.instances {
            let stack = instance.stack_id.as_ref().and_then(|id| {
                self.state.read().get(&instance.account).and_then(|s| {
                    s.stacks
                        .values()
                        .find(|st| &st.stack_id == id && st.status != "DELETE_COMPLETE")
                        .cloned()
                })
            });
            let Some(stack) = stack else {
                instance_drift.push((
                    instance.account.clone(),
                    instance.region.clone(),
                    "UNKNOWN".to_string(),
                ));
                op.results.push(OperationResult {
                    account: instance.account.clone(),
                    region: instance.region.clone(),
                    status: "FAILED".to_string(),
                    status_reason: Some("Stack instance does not have a stack".to_string()),
                    organizational_unit_id: instance.organizational_unit_id.clone(),
                    account_gate_status: None,
                    account_gate_reason: None,
                });
                continue;
            };
            let mut drifted = false;
            for resource in &stack.resources {
                let status = match self.resource_exists(&instance.account, resource) {
                    Some(true) => "IN_SYNC",
                    Some(false) => {
                        drifted = true;
                        "DELETED"
                    }
                    None => "NOT_CHECKED",
                };
                op.resource_drifts.push(InstanceResourceDrift {
                    account: instance.account.clone(),
                    region: instance.region.clone(),
                    stack_id: stack.stack_id.clone(),
                    logical_id: resource.logical_id.clone(),
                    physical_id: resource.physical_id.clone(),
                    resource_type: resource.resource_type.clone(),
                    status: status.to_string(),
                    timestamp: now,
                });
            }
            let status = if drifted { "DRIFTED" } else { "IN_SYNC" };
            instance_drift.push((
                instance.account.clone(),
                instance.region.clone(),
                status.to_string(),
            ));
            op.results.push(OperationResult {
                account: instance.account.clone(),
                region: instance.region.clone(),
                status: "SUCCEEDED".to_string(),
                status_reason: None,
                organizational_unit_id: instance.organizational_unit_id.clone(),
                account_gate_status: None,
                account_gate_reason: None,
            });
        }
        let drifted = instance_drift
            .iter()
            .filter(|(_, _, s)| s == "DRIFTED")
            .count();
        let in_sync = instance_drift
            .iter()
            .filter(|(_, _, s)| s == "IN_SYNC")
            .count();
        let failed = instance_drift
            .iter()
            .filter(|(_, _, s)| s == "UNKNOWN")
            .count();
        let details = DriftDetectionDetails {
            drift_status: if set.instances.is_empty() {
                "NOT_CHECKED".to_string()
            } else if drifted > 0 {
                "DRIFTED".to_string()
            } else {
                "IN_SYNC".to_string()
            },
            detection_status: if failed == 0 {
                "COMPLETED".to_string()
            } else if failed == instance_drift.len() {
                "FAILED".to_string()
            } else {
                "PARTIAL_SUCCESS".to_string()
            },
            last_drift_check_timestamp: now,
            total: set.instances.len(),
            drifted,
            in_sync,
            failed,
        };
        op.drift = Some(details.clone());
        op.status = settled_status(&op).to_string();
        op.ended_at = Some(Utc::now());

        let mut accounts = self.state.write();
        Self::refresh_stack_set(&mut accounts, &admin, &set.stack_set_id);
        if let Some(stored) = accounts
            .get_mut(&admin)
            .and_then(|s| s.stack_sets.get_mut(&set.stack_set_id))
        {
            // Re-validate against what is stored now: another operation may
            // have started, or used this OperationId, while resources were
            // being checked. (DetectStackSetDrift does not model
            // StaleRequestException, and a drift result stays valid after an
            // operation that has already finished.)
            Self::check_can_start_operation(stored, &op_id)?;
            for instance in &mut stored.instances {
                if let Some((_, _, status)) = instance_drift
                    .iter()
                    .find(|(a, r, _)| *a == instance.account && *r == instance.region)
                {
                    instance.drift_status = status.clone();
                    instance.last_drift_check_timestamp = Some(now);
                }
            }
            stored.drift = Some(details);
            stored.operations.push(op);
        }
        Ok(xml_response(
            "DetectStackSetDrift",
            el("OperationId", &op_id),
            &req.request_id,
        ))
    }

    fn list_stack_instance_resource_drifts(
        &self,
        req: &AwsRequest,
        params: &BTreeMap<String, String>,
    ) -> Result<AwsResponse, AwsServiceError> {
        let name = required(params, "StackSetName")?;
        let account = required(params, "StackInstanceAccount")?;
        let region = required(params, "StackInstanceRegion")?;
        let op_id = required(params, "OperationId")?;
        let admin = self.stack_set_admin_account(req, params)?;
        let set = self.read_stack_set(&admin, &name, Scope::of(params))?;
        let op = set
            .operations
            .iter()
            .find(|o| o.operation_id == op_id)
            .ok_or_else(|| operation_not_found(&op_id))?;
        if !set
            .instances
            .iter()
            .any(|i| i.account == account && i.region == region)
        {
            return Err(instance_not_found(&name, &account, &region));
        }
        let statuses = member_list(params, "StackInstanceResourceDriftStatuses");
        let drifts: Vec<&InstanceResourceDrift> = op
            .resource_drifts
            .iter()
            .filter(|d| d.account == account && d.region == region)
            .filter(|d| statuses.is_empty() || statuses.contains(&d.status))
            .collect();
        let (page, next) = paginate(drifts, params)?;
        let inner = format!(
            "{}{}",
            list_el(
                "Summaries",
                page.into_iter().map(|d| {
                    format!(
                        "{}{}{}{}<PropertyDifferences/>{}{}",
                        el("StackId", &d.stack_id),
                        el("LogicalResourceId", &d.logical_id),
                        el("PhysicalResourceId", &d.physical_id),
                        el("ResourceType", &d.resource_type),
                        el("StackResourceDriftStatus", &d.status),
                        el("Timestamp", &ts(&d.timestamp)),
                    )
                })
            ),
            next_token_el(next)
        );
        Ok(xml_response(
            "ListStackInstanceResourceDrifts",
            inner,
            &req.request_id,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extras::tests::{deps, req};
    use crate::service::CloudFormationDeps;
    use crate::state::SharedCloudFormationState;
    use fakecloud_core::delivery::{DeliveryBus, LambdaDelivery};
    use fakecloud_core::service::AwsService;
    use parking_lot::RwLock;
    use std::sync::Arc;

    const ADMIN: &str = "000000000000";
    const ACCT_B: &str = "111111111111";
    const ACCT_C: &str = "222222222222";
    // Unnamed resources are named after their logical id, so the queue is named
    // after its stack to keep instances in one account apart.
    const QUEUE_TEMPLATE: &str = "Parameters:\n  Env:\n    Type: String\n    Default: dev\nResources:\n  Q:\n    Type: AWS::SQS::Queue\n    Properties:\n      QueueName:\n        Fn::Sub: \"${AWS::StackName}-q\"\n";
    const TOPIC_TEMPLATE: &str = "Parameters:\n  Env:\n    Type: String\n    Default: dev\nResources:\n  T:\n    Type: AWS::SNS::Topic\n";

    fn service_with(deps: CloudFormationDeps) -> CloudFormationService {
        let state: SharedCloudFormationState = Arc::new(RwLock::new(MultiAccountState::<
            CloudFormationState,
        >::new(
            ADMIN, "us-east-1", ""
        )));
        CloudFormationService::new(state, deps)
    }

    fn service() -> CloudFormationService {
        service_with(deps())
    }

    async fn call_as(
        svc: &CloudFormationService,
        account: &str,
        action: &str,
        params: &[(&str, &str)],
    ) -> Result<String, AwsServiceError> {
        let mut request = req(action, params);
        request.account_id = account.to_string();
        let resp = svc.handle(request).await?;
        Ok(String::from_utf8(resp.body.expect_bytes().to_vec()).expect("utf8"))
    }

    async fn call(
        svc: &CloudFormationService,
        action: &str,
        params: &[(&str, &str)],
    ) -> Result<String, AwsServiceError> {
        call_as(svc, ADMIN, action, params).await
    }

    async fn ok(svc: &CloudFormationService, action: &str, params: &[(&str, &str)]) -> String {
        match call(svc, action, params).await {
            Ok(xml) => xml,
            Err(e) => panic!("{action} failed: {} {}", e.code(), e.message()),
        }
    }

    async fn err(
        svc: &CloudFormationService,
        action: &str,
        params: &[(&str, &str)],
    ) -> AwsServiceError {
        match call(svc, action, params).await {
            Ok(xml) => panic!("{action} should fail, got {xml}"),
            Err(e) => e,
        }
    }

    fn tag(xml: &str, name: &str) -> String {
        let open = format!("<{name}>");
        xml.split(&open)
            .nth(1)
            .and_then(|rest| rest.split(&format!("</{name}>")).next())
            .unwrap_or_else(|| panic!("no <{name}> in {xml}"))
            .to_string()
    }

    fn stored_set(svc: &CloudFormationService, name: &str) -> StackSet {
        svc.state
            .read()
            .get(ADMIN)
            .and_then(|s| find_active(s, name, Scope::Own))
            .cloned()
            .expect("stack set")
    }

    fn stack_of(svc: &CloudFormationService, account: &str, stack_id: &str) -> crate::state::Stack {
        svc.state
            .read()
            .get(account)
            .and_then(|s| {
                s.stacks
                    .values()
                    .find(|st| st.stack_id == stack_id)
                    .cloned()
            })
            .expect("instance stack")
    }

    fn queue_count(svc: &CloudFormationService, account: &str) -> usize {
        svc.deps
            .sqs
            .read()
            .get(account)
            .map_or(0, |s| s.queues.len())
    }

    async fn create_set(svc: &CloudFormationService, name: &str, template: &str) {
        ok(
            svc,
            "CreateStackSet",
            &[("StackSetName", name), ("TemplateBody", template)],
        )
        .await;
    }

    #[tokio::test]
    async fn stack_instances_provision_real_stacks_per_account_and_region() {
        let svc = service();
        create_set(&svc, "app", QUEUE_TEMPLATE).await;
        let xml = ok(
            &svc,
            "CreateStackInstances",
            &[
                ("StackSetName", "app"),
                ("Accounts.member.1", ACCT_B),
                ("Accounts.member.2", ACCT_C),
                ("Regions.member.1", "us-east-1"),
                ("Regions.member.2", "eu-west-1"),
                ("ParameterOverrides.member.1.ParameterKey", "Env"),
                ("ParameterOverrides.member.1.ParameterValue", "prod"),
            ],
        )
        .await;
        let op_id = tag(&xml, "OperationId");

        let op = ok(
            &svc,
            "DescribeStackSetOperation",
            &[("StackSetName", "app"), ("OperationId", &op_id)],
        )
        .await;
        assert_eq!(tag(&op, "Status"), "SUCCEEDED", "{op}");
        assert_eq!(tag(&op, "Action"), "CREATE");
        assert!(op.contains("<EndTimestamp>"), "{op}");

        let set = stored_set(&svc, "app");
        assert_eq!(set.instances.len(), 4);
        for instance in &set.instances {
            assert_eq!(instance.status, "CURRENT");
            assert_eq!(instance.detailed_status, "SUCCEEDED");
            let stack_id = instance.stack_id.as_deref().expect("stack id");
            assert!(
                stack_id.starts_with(&format!(
                    "arn:aws:cloudformation:{}:{}:stack/StackSet-app-",
                    instance.region, instance.account
                )),
                "{stack_id}"
            );
            let stack = stack_of(&svc, &instance.account, stack_id);
            assert_eq!(stack.status, "CREATE_COMPLETE");
            assert_eq!(
                stack.parameters.get("Env").map(String::as_str),
                Some("prod")
            );
            assert_eq!(stack.resources.len(), 1);
        }
        // Each account got a queue per region, in that account.
        assert_eq!(queue_count(&svc, ACCT_B), 2);
        assert_eq!(queue_count(&svc, ACCT_C), 2);
        assert_eq!(queue_count(&svc, ADMIN), 0);

        let described = ok(
            &svc,
            "DescribeStackInstance",
            &[
                ("StackSetName", "app"),
                ("StackInstanceAccount", ACCT_B),
                ("StackInstanceRegion", "eu-west-1"),
            ],
        )
        .await;
        assert_eq!(tag(&described, "Status"), "CURRENT");
        assert_eq!(tag(&described, "DetailedStatus"), "SUCCEEDED");
        assert_eq!(tag(&described, "LastOperationId"), op_id);
        assert!(
            described.contains("<ParameterValue>prod</ParameterValue>"),
            "{described}"
        );

        let listed = ok(
            &svc,
            "ListStackInstances",
            &[("StackSetName", "app"), ("StackInstanceAccount", ACCT_C)],
        )
        .await;
        assert_eq!(listed.matches("<member>").count(), 2, "{listed}");

        let results = ok(
            &svc,
            "ListStackSetOperationResults",
            &[("StackSetName", "app"), ("OperationId", &op_id)],
        )
        .await;
        assert_eq!(
            results.matches("<Status>SUCCEEDED</Status>").count(),
            4,
            "{results}"
        );
        assert!(
            results.contains("<AccountGateResult><Status>SKIPPED</Status>"),
            "{results}"
        );

        let summary = ok(&svc, "DescribeStackSet", &[("StackSetName", "app")]).await;
        assert!(summary.contains("<member>eu-west-1</member>"), "{summary}");
        assert!(summary.contains("<PermissionModel>SELF_MANAGED</PermissionModel>"));
        assert!(summary.contains(&format!(
            "<AdministrationRoleARN>arn:aws:iam::{ADMIN}:role/AWSCloudFormationStackSetAdministrationRole</AdministrationRoleARN>"
        )));
    }

    #[tokio::test]
    async fn updating_the_stack_set_redeploys_every_instance() {
        let svc = service();
        create_set(&svc, "app", QUEUE_TEMPLATE).await;
        ok(
            &svc,
            "CreateStackInstances",
            &[
                ("StackSetName", "app"),
                ("Accounts.member.1", ACCT_B),
                ("Regions.member.1", "us-east-1"),
                ("Regions.member.2", "us-west-2"),
            ],
        )
        .await;
        let xml = ok(
            &svc,
            "UpdateStackSet",
            &[("StackSetName", "app"), ("TemplateBody", TOPIC_TEMPLATE)],
        )
        .await;
        let op_id = tag(&xml, "OperationId");
        let op = ok(
            &svc,
            "DescribeStackSetOperation",
            &[("StackSetName", "app"), ("OperationId", &op_id)],
        )
        .await;
        assert_eq!(tag(&op, "Status"), "SUCCEEDED", "{op}");
        assert_eq!(tag(&op, "Action"), "UPDATE");

        let set = stored_set(&svc, "app");
        for instance in &set.instances {
            let stack = stack_of(&svc, ACCT_B, instance.stack_id.as_deref().unwrap());
            assert_eq!(stack.status, "UPDATE_COMPLETE");
            assert_eq!(stack.resources[0].resource_type, "AWS::SNS::Topic");
            assert_eq!(instance.last_operation_id.as_deref(), Some(op_id.as_str()));
        }
        // The queues the old template made are gone.
        assert_eq!(queue_count(&svc, ACCT_B), 0);

        let ops = ok(&svc, "ListStackSetOperations", &[("StackSetName", "app")]).await;
        assert_eq!(ops.matches("<member>").count(), 2, "{ops}");
        // Most recent first.
        assert_eq!(tag(&ops, "Action"), "UPDATE");
    }

    #[tokio::test]
    async fn a_partial_stack_set_update_leaves_the_rest_outdated() {
        let svc = service();
        create_set(&svc, "app", QUEUE_TEMPLATE).await;
        ok(
            &svc,
            "CreateStackInstances",
            &[
                ("StackSetName", "app"),
                ("Accounts.member.1", ACCT_B),
                ("Regions.member.1", "us-east-1"),
                ("Regions.member.2", "us-west-2"),
            ],
        )
        .await;
        ok(
            &svc,
            "UpdateStackSet",
            &[
                ("StackSetName", "app"),
                ("TemplateBody", TOPIC_TEMPLATE),
                ("Accounts.member.1", ACCT_B),
                ("Regions.member.1", "us-west-2"),
            ],
        )
        .await;
        let set = stored_set(&svc, "app");
        let east = set
            .instances
            .iter()
            .find(|i| i.region == "us-east-1")
            .unwrap();
        let west = set
            .instances
            .iter()
            .find(|i| i.region == "us-west-2")
            .unwrap();
        assert_eq!(west.status, "CURRENT");
        assert_eq!(east.status, "OUTDATED");
        assert_eq!(
            stack_of(&svc, ACCT_B, east.stack_id.as_deref().unwrap()).resources[0].resource_type,
            "AWS::SQS::Queue"
        );
    }

    #[tokio::test]
    async fn update_stack_instances_applies_and_keeps_overrides() {
        let template = "Parameters:\n  Env:\n    Type: String\n    Default: dev\n  Size:\n    Type: String\n    Default: s\nResources:\n  Q:\n    Type: AWS::SQS::Queue\n";
        let svc = service();
        create_set(&svc, "app", template).await;
        ok(
            &svc,
            "CreateStackInstances",
            &[
                ("StackSetName", "app"),
                ("Accounts.member.1", ACCT_B),
                ("Regions.member.1", "us-east-1"),
                ("ParameterOverrides.member.1.ParameterKey", "Env"),
                ("ParameterOverrides.member.1.ParameterValue", "prod"),
            ],
        )
        .await;
        ok(
            &svc,
            "UpdateStackInstances",
            &[
                ("StackSetName", "app"),
                ("Accounts.member.1", ACCT_B),
                ("Regions.member.1", "us-east-1"),
                ("ParameterOverrides.member.1.ParameterKey", "Env"),
                ("ParameterOverrides.member.1.UsePreviousValue", "true"),
                ("ParameterOverrides.member.2.ParameterKey", "Size"),
                ("ParameterOverrides.member.2.ParameterValue", "xl"),
            ],
        )
        .await;
        let set = stored_set(&svc, "app");
        let instance = &set.instances[0];
        assert_eq!(
            instance.parameter_overrides.get("Env").map(String::as_str),
            Some("prod")
        );
        assert_eq!(
            instance.parameter_overrides.get("Size").map(String::as_str),
            Some("xl")
        );
        let stack = stack_of(&svc, ACCT_B, instance.stack_id.as_deref().unwrap());
        assert_eq!(stack.parameters.get("Size").map(String::as_str), Some("xl"));
        assert_eq!(
            stack.parameters.get("Env").map(String::as_str),
            Some("prod")
        );

        // Leaving a parameter out of the list reverts it to the stack set's value.
        ok(
            &svc,
            "UpdateStackInstances",
            &[
                ("StackSetName", "app"),
                ("Accounts.member.1", ACCT_B),
                ("Regions.member.1", "us-east-1"),
                ("ParameterOverrides.member.1.ParameterKey", "Size"),
                ("ParameterOverrides.member.1.UsePreviousValue", "true"),
            ],
        )
        .await;
        let set = stored_set(&svc, "app");
        let stack = stack_of(&svc, ACCT_B, set.instances[0].stack_id.as_deref().unwrap());
        assert_eq!(stack.parameters.get("Env").map(String::as_str), Some("dev"));
        assert_eq!(stack.parameters.get("Size").map(String::as_str), Some("xl"));

        // Overriding a parameter the template does not declare is rejected.
        let e = err(
            &svc,
            "UpdateStackInstances",
            &[
                ("StackSetName", "app"),
                ("Accounts.member.1", ACCT_B),
                ("Regions.member.1", "us-east-1"),
                ("ParameterOverrides.member.1.ParameterKey", "Nope"),
                ("ParameterOverrides.member.1.ParameterValue", "x"),
            ],
        )
        .await;
        assert_eq!(e.code(), "ValidationError");

        // An instance that does not exist is reported.
        let e = err(
            &svc,
            "UpdateStackInstances",
            &[
                ("StackSetName", "app"),
                ("Accounts.member.1", ACCT_C),
                ("Regions.member.1", "us-east-1"),
            ],
        )
        .await;
        assert_eq!(e.code(), "StackInstanceNotFoundException");
    }

    #[tokio::test]
    async fn deleting_instances_tears_down_or_retains_their_stacks() {
        let svc = service();
        create_set(&svc, "app", QUEUE_TEMPLATE).await;
        ok(
            &svc,
            "CreateStackInstances",
            &[
                ("StackSetName", "app"),
                ("Accounts.member.1", ACCT_B),
                ("Regions.member.1", "us-east-1"),
                ("Regions.member.2", "us-west-2"),
            ],
        )
        .await;
        let e = err(&svc, "DeleteStackSet", &[("StackSetName", "app")]).await;
        assert_eq!(e.code(), "StackSetNotEmptyException");

        let set = stored_set(&svc, "app");
        let east_stack = set
            .instances
            .iter()
            .find(|i| i.region == "us-east-1")
            .unwrap()
            .stack_id
            .clone()
            .unwrap();
        let west_stack = set
            .instances
            .iter()
            .find(|i| i.region == "us-west-2")
            .unwrap()
            .stack_id
            .clone()
            .unwrap();

        ok(
            &svc,
            "DeleteStackInstances",
            &[
                ("StackSetName", "app"),
                ("Accounts.member.1", ACCT_B),
                ("Regions.member.1", "us-east-1"),
                ("RetainStacks", "false"),
            ],
        )
        .await;
        assert_eq!(
            stack_of(&svc, ACCT_B, &east_stack).status,
            "DELETE_COMPLETE"
        );
        assert_eq!(queue_count(&svc, ACCT_B), 1);

        ok(
            &svc,
            "DeleteStackInstances",
            &[
                ("StackSetName", "app"),
                ("Accounts.member.1", ACCT_B),
                ("Regions.member.1", "us-west-2"),
                ("RetainStacks", "true"),
            ],
        )
        .await;
        // Retained: the stack and its queue outlive the instance.
        assert_eq!(
            stack_of(&svc, ACCT_B, &west_stack).status,
            "CREATE_COMPLETE"
        );
        assert_eq!(queue_count(&svc, ACCT_B), 1);
        assert!(stored_set(&svc, "app").instances.is_empty());

        let e = err(
            &svc,
            "DescribeStackInstance",
            &[
                ("StackSetName", "app"),
                ("StackInstanceAccount", ACCT_B),
                ("StackInstanceRegion", "us-east-1"),
            ],
        )
        .await;
        assert_eq!(e.code(), "StackInstanceNotFoundException");

        // Empty now, so it deletes; the name is free and the id still resolves.
        let id = stored_set(&svc, "app").stack_set_id;
        ok(&svc, "DeleteStackSet", &[("StackSetName", "app")]).await;
        let e = err(&svc, "DescribeStackSet", &[("StackSetName", "app")]).await;
        assert_eq!(e.code(), "StackSetNotFoundException");
        let by_id = ok(&svc, "DescribeStackSet", &[("StackSetName", &id)]).await;
        assert_eq!(tag(&by_id, "Status"), "DELETED");
        let deleted = ok(&svc, "ListStackSets", &[("Status", "DELETED")]).await;
        assert!(deleted.contains(&id), "{deleted}");
        let active = ok(&svc, "ListStackSets", &[("Status", "ACTIVE")]).await;
        assert!(!active.contains(&id), "{active}");
        create_set(&svc, "app", QUEUE_TEMPLATE).await;
    }

    #[tokio::test]
    async fn a_failing_target_cancels_the_rest_beyond_the_tolerance() {
        // A template importing an export that does not exist fails to create.
        let broken = "Resources:\n  Q:\n    Type: AWS::SQS::Queue\n    Properties:\n      QueueName:\n        Fn::ImportValue: missing-export\n";
        let svc = service();
        create_set(&svc, "bad", broken).await;
        let xml = ok(
            &svc,
            "CreateStackInstances",
            &[
                ("StackSetName", "bad"),
                ("Accounts.member.1", ACCT_B),
                ("Regions.member.1", "us-east-1"),
                ("Regions.member.2", "us-west-2"),
            ],
        )
        .await;
        let op_id = tag(&xml, "OperationId");
        let op = ok(
            &svc,
            "DescribeStackSetOperation",
            &[("StackSetName", "bad"), ("OperationId", &op_id)],
        )
        .await;
        assert_eq!(tag(&op, "Status"), "FAILED", "{op}");
        assert_eq!(tag(&op, "FailedStackInstancesCount"), "1");

        let results = ok(
            &svc,
            "ListStackSetOperationResults",
            &[
                ("StackSetName", "bad"),
                ("OperationId", &op_id),
                ("Filters.member.1.Name", "OPERATION_RESULT_STATUS"),
                ("Filters.member.1.Values", "CANCELLED"),
            ],
        )
        .await;
        assert!(results.contains("<Region>us-west-2</Region>"), "{results}");
        assert!(results.contains(TOLERANCE_EXCEEDED), "{results}");

        let set = stored_set(&svc, "bad");
        let failed = set
            .instances
            .iter()
            .find(|i| i.region == "us-east-1")
            .unwrap();
        assert_eq!(failed.detailed_status, "FAILED");
        assert!(failed
            .status_reason
            .as_deref()
            .unwrap()
            .contains("missing-export"));
        let cancelled = set
            .instances
            .iter()
            .find(|i| i.region == "us-west-2")
            .unwrap();
        assert_eq!(cancelled.detailed_status, "CANCELLED");

        let filtered = ok(
            &svc,
            "ListStackInstances",
            &[
                ("StackSetName", "bad"),
                ("Filters.member.1.Name", "DETAILED_STATUS"),
                ("Filters.member.1.Values", "FAILED"),
            ],
        )
        .await;
        assert_eq!(filtered.matches("<member>").count(), 1, "{filtered}");

        // With one failure tolerated per region, both regions are attempted
        // and the operation succeeds despite the failures being per-region 1.
        let xml = ok(
            &svc,
            "CreateStackInstances",
            &[
                ("StackSetName", "bad"),
                ("Accounts.member.1", ACCT_C),
                ("Regions.member.1", "us-east-1"),
                ("Regions.member.2", "us-west-2"),
                ("OperationPreferences.FailureToleranceCount", "1"),
            ],
        )
        .await;
        let op = ok(
            &svc,
            "DescribeStackSetOperation",
            &[
                ("StackSetName", "bad"),
                ("OperationId", &tag(&xml, "OperationId")),
            ],
        )
        .await;
        assert_eq!(tag(&op, "Status"), "SUCCEEDED", "{op}");
        assert_eq!(tag(&op, "FailedStackInstancesCount"), "2");
    }

    #[tokio::test]
    async fn operations_report_modeled_errors() {
        let svc = service();
        let e = err(
            &svc,
            "CreateStackInstances",
            &[
                ("StackSetName", "nope"),
                ("Accounts.member.1", ACCT_B),
                ("Regions.member.1", "us-east-1"),
            ],
        )
        .await;
        assert_eq!(e.code(), "StackSetNotFoundException");
        assert_eq!(e.status(), StatusCode::NOT_FOUND);

        create_set(&svc, "app", QUEUE_TEMPLATE).await;
        let e = err(
            &svc,
            "CreateStackSet",
            &[("StackSetName", "app"), ("TemplateBody", QUEUE_TEMPLATE)],
        )
        .await;
        assert_eq!(e.code(), "NameAlreadyExistsException");

        let e = err(
            &svc,
            "DescribeStackSetOperation",
            &[("StackSetName", "app"), ("OperationId", "never")],
        )
        .await;
        assert_eq!(e.code(), "OperationNotFoundException");

        let params = [
            ("StackSetName", "app"),
            ("Accounts.member.1", ACCT_B),
            ("Regions.member.1", "us-east-1"),
            ("OperationId", "op-1"),
        ];
        ok(&svc, "CreateStackInstances", &params).await;
        let e = err(&svc, "CreateStackInstances", &params).await;
        assert_eq!(e.code(), "OperationIdAlreadyExistsException");

        let e = err(
            &svc,
            "StopStackSetOperation",
            &[("StackSetName", "app"), ("OperationId", "op-1")],
        )
        .await;
        assert_eq!(e.code(), "InvalidOperationException");

        let e = err(
            &svc,
            "CreateStackInstances",
            &[
                ("StackSetName", "app"),
                ("Accounts.member.1", "not-an-account"),
                ("Regions.member.1", "us-east-1"),
            ],
        )
        .await;
        assert_eq!(e.code(), "ValidationError");
        let e = err(
            &svc,
            "CreateStackInstances",
            &[("StackSetName", "app"), ("Accounts.member.1", ACCT_B)],
        )
        .await;
        assert_eq!(e.code(), "ValidationError");
    }

    #[tokio::test]
    async fn a_running_operation_blocks_others_and_can_be_stopped() {
        let svc = service();
        create_set(&svc, "app", QUEUE_TEMPLATE).await;
        {
            let mut accounts = svc.state.write();
            let set = accounts
                .get_or_create(ADMIN)
                .stack_sets
                .values_mut()
                .next()
                .unwrap();
            let mut op = CloudFormationService::new_operation(
                &set.clone(),
                "running-op",
                "CREATE",
                OperationPreferences::default(),
                None,
                None,
            );
            op.results.push(OperationResult {
                account: ACCT_B.to_string(),
                region: "us-east-1".to_string(),
                status: "PENDING".to_string(),
                status_reason: None,
                organizational_unit_id: None,
                account_gate_status: None,
                account_gate_reason: None,
            });
            set.operations.push(op);
        }
        let e = err(
            &svc,
            "CreateStackInstances",
            &[
                ("StackSetName", "app"),
                ("Accounts.member.1", ACCT_B),
                ("Regions.member.1", "us-east-1"),
            ],
        )
        .await;
        assert_eq!(e.code(), "OperationInProgressException");
        let e = err(&svc, "DeleteStackSet", &[("StackSetName", "app")]).await;
        assert_eq!(e.code(), "OperationInProgressException");

        ok(
            &svc,
            "StopStackSetOperation",
            &[("StackSetName", "app"), ("OperationId", "running-op")],
        )
        .await;
        let op = ok(
            &svc,
            "DescribeStackSetOperation",
            &[("StackSetName", "app"), ("OperationId", "running-op")],
        )
        .await;
        assert_eq!(tag(&op, "Status"), "STOPPED", "{op}");
        let results = ok(
            &svc,
            "ListStackSetOperationResults",
            &[("StackSetName", "app"), ("OperationId", "running-op")],
        )
        .await;
        assert_eq!(tag(&results, "Status"), "CANCELLED", "{results}");
    }

    #[tokio::test]
    async fn an_asynchronously_provisioning_instance_settles_on_read() {
        let svc = service();
        create_set(&svc, "app", QUEUE_TEMPLATE).await;
        ok(
            &svc,
            "CreateStackInstances",
            &[
                ("StackSetName", "app"),
                ("Accounts.member.1", ACCT_B),
                ("Regions.member.1", "us-east-1"),
                ("OperationId", "op-async"),
            ],
        )
        .await;
        // Rewind to the moment the stack was still provisioning.
        let stack_id = {
            let mut accounts = svc.state.write();
            let set = accounts
                .get_or_create(ADMIN)
                .stack_sets
                .values_mut()
                .next()
                .unwrap();
            set.instances[0].detailed_status = "RUNNING".to_string();
            set.instances[0].status = "OUTDATED".to_string();
            let op = set.operations.last_mut().unwrap();
            op.status = "RUNNING".to_string();
            op.ended_at = None;
            op.results[0].status = "RUNNING".to_string();
            let stack_id = set.instances[0].stack_id.clone().unwrap();
            let stack = accounts
                .get_or_create(ACCT_B)
                .stacks
                .values_mut()
                .find(|s| s.stack_id == stack_id)
                .unwrap();
            stack.status = "CREATE_IN_PROGRESS".to_string();
            stack_id
        };
        let op = ok(
            &svc,
            "DescribeStackSetOperation",
            &[("StackSetName", "app"), ("OperationId", "op-async")],
        )
        .await;
        assert_eq!(tag(&op, "Status"), "RUNNING", "{op}");

        // The stack finishes: the next read reflects it.
        svc.state
            .write()
            .get_or_create(ACCT_B)
            .stacks
            .values_mut()
            .find(|s| s.stack_id == stack_id)
            .unwrap()
            .status = "CREATE_COMPLETE".to_string();
        let op = ok(
            &svc,
            "DescribeStackSetOperation",
            &[("StackSetName", "app"), ("OperationId", "op-async")],
        )
        .await;
        assert_eq!(tag(&op, "Status"), "SUCCEEDED", "{op}");
        let instance = ok(
            &svc,
            "DescribeStackInstance",
            &[
                ("StackSetName", "app"),
                ("StackInstanceAccount", ACCT_B),
                ("StackInstanceRegion", "us-east-1"),
            ],
        )
        .await;
        assert_eq!(tag(&instance, "Status"), "CURRENT");
    }

    fn seed_org(svc: &CloudFormationService) -> (String, String) {
        let mut org = fakecloud_organizations::OrganizationState::bootstrap(ADMIN);
        let root = org.root_id.clone();
        let parent = org.create_ou(&root, "workloads").unwrap();
        let child = org.create_ou(&parent.id, "prod").unwrap();
        for (account, dest) in [(ACCT_B, &parent.id), (ACCT_C, &child.id)] {
            org.enroll_account_if_missing(account);
            org.move_account(account, &root, dest).unwrap();
        }
        *svc.deps.organizations.write() = Some(org);
        (parent.id, child.id)
    }

    #[tokio::test]
    async fn service_managed_stack_sets_deploy_to_organizational_units() {
        let svc = service();
        let (workloads, prod) = seed_org(&svc);
        let create = [
            ("StackSetName", "org"),
            ("TemplateBody", QUEUE_TEMPLATE),
            ("PermissionModel", "SERVICE_MANAGED"),
            ("AutoDeployment.Enabled", "true"),
            ("AutoDeployment.RetainStacksOnAccountRemoval", "false"),
        ];
        // Trusted access has to be activated first.
        let e = err(&svc, "CreateStackSet", &create).await;
        assert_eq!(e.code(), "ValidationError");
        ok(&svc, "ActivateOrganizationsAccess", &[]).await;
        ok(&svc, "CreateStackSet", &create).await;

        // Top-level accounts are not a valid target for this model.
        let e = err(
            &svc,
            "CreateStackInstances",
            &[
                ("StackSetName", "org"),
                ("Accounts.member.1", ACCT_B),
                ("Regions.member.1", "us-east-1"),
            ],
        )
        .await;
        assert_eq!(e.code(), "ValidationError");

        // The OU covers its nested OU; the management account is never a target.
        ok(
            &svc,
            "CreateStackInstances",
            &[
                ("StackSetName", "org"),
                (
                    "DeploymentTargets.OrganizationalUnitIds.member.1",
                    &workloads,
                ),
                ("Regions.member.1", "us-east-1"),
            ],
        )
        .await;
        let set = stored_set(&svc, "org");
        let mut accounts: Vec<&str> = set.instances.iter().map(|i| i.account.as_str()).collect();
        accounts.sort();
        assert_eq!(accounts, [ACCT_B, ACCT_C]);
        assert!(set
            .instances
            .iter()
            .all(|i| i.organizational_unit_id.as_deref() == Some(workloads.as_str())));
        assert_eq!(queue_count(&svc, ACCT_C), 1);

        let targets = ok(
            &svc,
            "ListStackSetAutoDeploymentTargets",
            &[("StackSetName", "org")],
        )
        .await;
        assert_eq!(tag(&targets, "OrganizationalUnitId"), workloads);
        assert!(targets.contains("<member>us-east-1</member>"), "{targets}");
        let described = ok(&svc, "DescribeStackSet", &[("StackSetName", "org")]).await;
        assert!(
            described.contains(&format!("<member>{workloads}</member>")),
            "{described}"
        );
        assert!(described.contains("<Enabled>true</Enabled>"), "{described}");

        // DIFFERENCE removes the listed account from the OU's instances.
        ok(
            &svc,
            "DeleteStackInstances",
            &[
                ("StackSetName", "org"),
                (
                    "DeploymentTargets.OrganizationalUnitIds.member.1",
                    &workloads,
                ),
                ("DeploymentTargets.Accounts.member.1", ACCT_B),
                ("DeploymentTargets.AccountFilterType", "DIFFERENCE"),
                ("Regions.member.1", "us-east-1"),
                ("RetainStacks", "false"),
            ],
        )
        .await;
        let set = stored_set(&svc, "org");
        assert_eq!(set.instances.len(), 1);
        assert_eq!(set.instances[0].account, ACCT_B);
        assert_eq!(queue_count(&svc, ACCT_C), 0);

        // A nested OU targets only its own accounts.
        ok(
            &svc,
            "CreateStackInstances",
            &[
                ("StackSetName", "org"),
                ("DeploymentTargets.OrganizationalUnitIds.member.1", &prod),
                ("Regions.member.1", "eu-west-1"),
            ],
        )
        .await;
        let set = stored_set(&svc, "org");
        let eu: Vec<&StackInstance> = set
            .instances
            .iter()
            .filter(|i| i.region == "eu-west-1")
            .collect();
        assert_eq!(eu.len(), 1);
        assert_eq!(eu[0].account, ACCT_C);

        // Instances deployed through a nested OU are reached through a parent.
        ok(
            &svc,
            "DeleteStackInstances",
            &[
                ("StackSetName", "org"),
                (
                    "DeploymentTargets.OrganizationalUnitIds.member.1",
                    &workloads,
                ),
                ("Regions.member.1", "eu-west-1"),
                ("RetainStacks", "false"),
            ],
        )
        .await;
        assert!(stored_set(&svc, "org")
            .instances
            .iter()
            .all(|i| i.region != "eu-west-1"));

        let e = err(
            &svc,
            "CreateStackInstances",
            &[
                ("StackSetName", "org"),
                (
                    "DeploymentTargets.OrganizationalUnitIds.member.1",
                    "ou-none-00000000",
                ),
                ("Regions.member.1", "us-east-1"),
            ],
        )
        .await;
        assert_eq!(e.code(), "ValidationError");
    }

    #[tokio::test]
    async fn a_delegated_administrator_acts_on_the_management_accounts_stack_sets() {
        let svc = service();
        seed_org(&svc);
        {
            let mut orgs = svc.deps.organizations.write();
            let org = orgs.as_mut().unwrap();
            org.enable_aws_service_access(STACKSETS_PRINCIPAL);
            org.register_delegated_administrator(ACCT_B, STACKSETS_PRINCIPAL)
                .unwrap();
        }
        let params = [
            ("StackSetName", "delegated"),
            ("TemplateBody", QUEUE_TEMPLATE),
            ("PermissionModel", "SERVICE_MANAGED"),
            ("CallAs", "DELEGATED_ADMIN"),
        ];
        call_as(&svc, ACCT_B, "CreateStackSet", &params)
            .await
            .unwrap();
        // Stored with the management account, visible to it too.
        assert_eq!(
            stored_set(&svc, "delegated").permission_model,
            "SERVICE_MANAGED"
        );
        let listed = call_as(
            &svc,
            ACCT_B,
            "ListStackSets",
            &[("CallAs", "DELEGATED_ADMIN")],
        )
        .await
        .unwrap();
        assert!(
            listed.contains("<StackSetName>delegated</StackSetName>"),
            "{listed}"
        );

        // An account that is not registered cannot.
        let e = call_as(
            &svc,
            ACCT_C,
            "ListStackSets",
            &[("CallAs", "DELEGATED_ADMIN")],
        )
        .await
        .unwrap_err();
        assert_eq!(e.code(), "ValidationError");
    }

    #[tokio::test]
    async fn existing_stacks_import_into_a_stack_set() {
        let svc = service();
        let xml = call_as(
            &svc,
            ACCT_B,
            "CreateStack",
            &[("StackName", "legacy"), ("TemplateBody", QUEUE_TEMPLATE)],
        )
        .await
        .unwrap();
        let stack_id = tag(&xml, "StackId");
        create_set(&svc, "adopt", QUEUE_TEMPLATE).await;

        let xml = ok(
            &svc,
            "ImportStacksToStackSet",
            &[("StackSetName", "adopt"), ("StackIds.member.1", &stack_id)],
        )
        .await;
        let op_id = tag(&xml, "OperationId");
        let op = ok(
            &svc,
            "DescribeStackSetOperation",
            &[("StackSetName", "adopt"), ("OperationId", &op_id)],
        )
        .await;
        assert_eq!(tag(&op, "Status"), "SUCCEEDED", "{op}");
        let set = stored_set(&svc, "adopt");
        assert_eq!(set.instances.len(), 1);
        assert_eq!(
            set.instances[0].stack_id.as_deref(),
            Some(stack_id.as_str())
        );
        assert_eq!(set.instances[0].account, ACCT_B);
        assert_eq!(set.instances[0].status, "CURRENT");

        // A stack can belong to one stack set only.
        create_set(&svc, "other", QUEUE_TEMPLATE).await;
        let xml = ok(
            &svc,
            "ImportStacksToStackSet",
            &[("StackSetName", "other"), ("StackIds.member.1", &stack_id)],
        )
        .await;
        let op = ok(
            &svc,
            "DescribeStackSetOperation",
            &[
                ("StackSetName", "other"),
                ("OperationId", &tag(&xml, "OperationId")),
            ],
        )
        .await;
        assert_eq!(tag(&op, "Status"), "FAILED", "{op}");
        assert!(stored_set(&svc, "other").instances.is_empty());

        let e = err(
            &svc,
            "ImportStacksToStackSet",
            &[
                ("StackSetName", "other"),
                (
                    "StackIds.member.1",
                    "arn:aws:cloudformation:us-east-1:111111111111:stack/ghost/1",
                ),
            ],
        )
        .await;
        assert_eq!(e.code(), "StackNotFoundException");

        // A stack set can also be created from a stack's template.
        let xml = call_as(
            &svc,
            ACCT_B,
            "CreateStackSet",
            &[("StackSetName", "from-stack"), ("StackId", &stack_id)],
        )
        .await
        .unwrap();
        assert!(xml.contains("<StackSetId>from-stack:"), "{xml}");
        let described = call_as(
            &svc,
            ACCT_B,
            "DescribeStackSet",
            &[("StackSetName", "from-stack")],
        )
        .await
        .unwrap();
        assert!(described.contains("AWS::SQS::Queue"), "{described}");
    }

    #[tokio::test]
    async fn stack_set_drift_detection_checks_each_instance() {
        let svc = service();
        create_set(&svc, "app", QUEUE_TEMPLATE).await;
        ok(
            &svc,
            "CreateStackInstances",
            &[
                ("StackSetName", "app"),
                ("Accounts.member.1", ACCT_B),
                ("Regions.member.1", "us-east-1"),
                ("Regions.member.2", "us-west-2"),
            ],
        )
        .await;
        // Delete one instance's queue behind CloudFormation's back.
        let set = stored_set(&svc, "app");
        let east = set
            .instances
            .iter()
            .find(|i| i.region == "us-east-1")
            .unwrap();
        let queue_url = stack_of(&svc, ACCT_B, east.stack_id.as_deref().unwrap()).resources[0]
            .physical_id
            .clone();
        svc.deps
            .sqs
            .write()
            .get_or_create(ACCT_B)
            .queues
            .remove(&queue_url)
            .expect("queue existed");

        let xml = ok(&svc, "DetectStackSetDrift", &[("StackSetName", "app")]).await;
        let op_id = tag(&xml, "OperationId");
        let op = ok(
            &svc,
            "DescribeStackSetOperation",
            &[("StackSetName", "app"), ("OperationId", &op_id)],
        )
        .await;
        assert_eq!(tag(&op, "Action"), "DETECT_DRIFT");
        assert_eq!(tag(&op, "DriftStatus"), "DRIFTED", "{op}");
        assert_eq!(tag(&op, "DriftedStackInstancesCount"), "1");
        assert_eq!(tag(&op, "InSyncStackInstancesCount"), "1");

        let drifts = ok(
            &svc,
            "ListStackInstanceResourceDrifts",
            &[
                ("StackSetName", "app"),
                ("StackInstanceAccount", ACCT_B),
                ("StackInstanceRegion", "us-east-1"),
                ("OperationId", &op_id),
                ("StackInstanceResourceDriftStatuses.member.1", "DELETED"),
            ],
        )
        .await;
        assert_eq!(
            tag(&drifts, "StackResourceDriftStatus"),
            "DELETED",
            "{drifts}"
        );
        assert_eq!(tag(&drifts, "PhysicalResourceId"), queue_url);

        let in_sync = ok(
            &svc,
            "ListStackInstances",
            &[
                ("StackSetName", "app"),
                ("Filters.member.1.Name", "DRIFT_STATUS"),
                ("Filters.member.1.Values", "IN_SYNC"),
            ],
        )
        .await;
        assert!(in_sync.contains("<Region>us-west-2</Region>"), "{in_sync}");
        assert!(!in_sync.contains("<Region>us-east-1</Region>"), "{in_sync}");
        let summary = ok(&svc, "ListStackSets", &[]).await;
        assert!(
            summary.contains("<DriftStatus>DRIFTED</DriftStatus>"),
            "{summary}"
        );
    }

    /// An account gate that stops the operation it is gating, standing in for
    /// a StopStackSetOperation that lands while a target is deploying.
    struct StoppingGate(std::sync::OnceLock<Arc<CloudFormationService>>);

    impl LambdaDelivery for StoppingGate {
        fn invoke_lambda(
            &self,
            _function_arn: &str,
            _payload: &str,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<u8>, String>> + Send>>
        {
            let svc = self.0.get().cloned();
            Box::pin(async move {
                if let Some(svc) = svc {
                    call(
                        &svc,
                        "StopStackSetOperation",
                        &[("StackSetName", "app"), ("OperationId", "op-stop")],
                    )
                    .await
                    .map_err(|e| e.message())?;
                }
                Ok(br#"{"Status":"SUCCEEDED"}"#.to_vec())
            })
        }
    }

    #[tokio::test]
    async fn stopping_an_operation_mid_deployment_cancels_the_remaining_targets() {
        let gate = Arc::new(StoppingGate(std::sync::OnceLock::new()));
        let mut d = deps();
        d.delivery = Arc::new(DeliveryBus::new().with_lambda(gate.clone()));
        let svc = Arc::new(service_with(d));
        gate.0.set(svc.clone()).ok();
        // Only the first target's account has a gate, so the stop lands while
        // that target is deploying.
        let gate_template = "Resources:\n  Gate:\n    Type: AWS::Lambda::Function\n    Properties:\n      FunctionName: AWSCloudFormationStackSetAccountGate\n      Runtime: python3.12\n      Handler: index.handler\n      Role: arn:aws:iam::111111111111:role/gate\n      Code:\n        ZipFile: \"def handler(e, c): return {}\"\n";
        call_as(
            &svc,
            ACCT_B,
            "CreateStack",
            &[("StackName", "gate"), ("TemplateBody", gate_template)],
        )
        .await
        .unwrap();
        create_set(&svc, "app", QUEUE_TEMPLATE).await;
        ok(
            &svc,
            "CreateStackInstances",
            &[
                ("StackSetName", "app"),
                ("Accounts.member.1", ACCT_B),
                ("Accounts.member.2", ACCT_C),
                ("Regions.member.1", "us-east-1"),
                ("OperationId", "op-stop"),
            ],
        )
        .await;
        let op = ok(
            &svc,
            "DescribeStackSetOperation",
            &[("StackSetName", "app"), ("OperationId", "op-stop")],
        )
        .await;
        assert_eq!(tag(&op, "Status"), "STOPPED", "{op}");
        let results = ok(
            &svc,
            "ListStackSetOperationResults",
            &[("StackSetName", "app"), ("OperationId", "op-stop")],
        )
        .await;
        // The target already deploying finishes; the next one never starts.
        assert_eq!(queue_count(&svc, ACCT_B), 1);
        assert_eq!(queue_count(&svc, ACCT_C), 0);
        assert!(results.contains("<Status>SUCCEEDED</Status>"), "{results}");
        assert!(results.contains(OPERATION_STOPPED), "{results}");
    }

    #[tokio::test]
    async fn a_delegated_administrator_cannot_touch_self_managed_stack_sets() {
        let svc = service();
        seed_org(&svc);
        {
            let mut orgs = svc.deps.organizations.write();
            let org = orgs.as_mut().unwrap();
            org.enable_aws_service_access(STACKSETS_PRINCIPAL);
            org.register_delegated_administrator(ACCT_B, STACKSETS_PRINCIPAL)
                .unwrap();
        }
        create_set(&svc, "mgmt-only", QUEUE_TEMPLATE).await;
        let delegated = [("StackSetName", "mgmt-only"), ("CallAs", "DELEGATED_ADMIN")];
        let e = call_as(&svc, ACCT_B, "DescribeStackSet", &delegated)
            .await
            .unwrap_err();
        assert_eq!(e.code(), "StackSetNotFoundException");
        let mut instances = delegated.to_vec();
        instances.extend([
            ("Accounts.member.1", ACCT_C),
            ("Regions.member.1", "us-east-1"),
        ]);
        let e = call_as(&svc, ACCT_B, "CreateStackInstances", &instances)
            .await
            .unwrap_err();
        assert_eq!(e.code(), "StackSetNotFoundException");
        assert_eq!(queue_count(&svc, ACCT_C), 0);
        let e = call_as(
            &svc,
            ACCT_B,
            "CreateStackSet",
            &[
                ("StackSetName", "sneaky"),
                ("TemplateBody", QUEUE_TEMPLATE),
                ("CallAs", "DELEGATED_ADMIN"),
            ],
        )
        .await
        .unwrap_err();
        assert_eq!(e.code(), "ValidationError");
        let listed = call_as(
            &svc,
            ACCT_B,
            "ListStackSets",
            &[("CallAs", "DELEGATED_ADMIN")],
        )
        .await
        .unwrap();
        assert!(!listed.contains("mgmt-only"), "{listed}");
    }

    #[tokio::test]
    async fn an_out_of_range_next_token_is_rejected() {
        let svc = service();
        create_set(&svc, "app", QUEUE_TEMPLATE).await;
        let e = err(
            &svc,
            "ListStackSets",
            &[("NextToken", "18446744073709551615")],
        )
        .await;
        assert_eq!(e.code(), "ValidationError");
        let page = ok(&svc, "ListStackSets", &[("MaxResults", "1")]).await;
        assert!(!page.contains("<NextToken>"), "{page}");
    }

    struct Gate(&'static str);

    impl LambdaDelivery for Gate {
        fn invoke_lambda(
            &self,
            _function_arn: &str,
            _payload: &str,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<u8>, String>> + Send>>
        {
            let body = format!("{{\"Status\":\"{}\"}}", self.0);
            Box::pin(async move { Ok(body.into_bytes()) })
        }
    }

    #[tokio::test]
    async fn an_account_gate_that_fails_blocks_deployment_to_that_account() {
        let mut d = deps();
        d.delivery = Arc::new(DeliveryBus::new().with_lambda(Arc::new(Gate("FAILED"))));
        let svc = service_with(d);
        let gate_template = "Resources:\n  Gate:\n    Type: AWS::Lambda::Function\n    Properties:\n      FunctionName: AWSCloudFormationStackSetAccountGate\n      Runtime: python3.12\n      Handler: index.handler\n      Role: arn:aws:iam::111111111111:role/gate\n      Code:\n        ZipFile: \"def handler(e, c): return {}\"\n";
        call_as(
            &svc,
            ACCT_B,
            "CreateStack",
            &[("StackName", "gate"), ("TemplateBody", gate_template)],
        )
        .await
        .unwrap();
        assert!(svc
            .deps
            .lambda
            .read()
            .get(ACCT_B)
            .is_some_and(|s| s.functions.contains_key(ACCOUNT_GATE_FUNCTION)));

        create_set(&svc, "gated", QUEUE_TEMPLATE).await;
        let xml = ok(
            &svc,
            "CreateStackInstances",
            &[
                ("StackSetName", "gated"),
                ("Accounts.member.1", ACCT_B),
                ("Accounts.member.2", ACCT_C),
                ("Regions.member.1", "us-east-1"),
                ("OperationPreferences.FailureToleranceCount", "1"),
            ],
        )
        .await;
        let results = ok(
            &svc,
            "ListStackSetOperationResults",
            &[
                ("StackSetName", "gated"),
                ("OperationId", &tag(&xml, "OperationId")),
            ],
        )
        .await;
        assert!(
            results.contains("<AccountGateResult><Status>FAILED</Status>"),
            "{results}"
        );
        assert_eq!(queue_count(&svc, ACCT_B), 0);
        // The ungated account deploys.
        assert_eq!(queue_count(&svc, ACCT_C), 1);
    }
}
