//! Layer ARNs and cross-account layer access.
//!
//! Every path that addresses a layer by ARN (the layer API, function layer
//! attachments, the invoke-time `/opt` mount, the admin artifact endpoint and
//! the CloudFormation provisioner) goes through [`LayerArn::parse`] and
//! [`find_layer_version`], so the account and region a layer ARN names are
//! read the same way everywhere.
//!
//! A layer version is private to its owner unless its resource-based policy
//! (written by `AddLayerVersionPermission`) grants `lambda:GetLayerVersion` to
//! the caller: an account, every account (`*`), or every account of an
//! organization (`*` plus an `aws:PrincipalOrgID` condition).

use std::sync::{Arc, OnceLock};

use fakecloud_core::auth::ScpResolver;
use fakecloud_core::multi_account::MultiRegionState;
use fakecloud_core::service::{AwsRequest, AwsServiceError};
use http::StatusCode;
use serde_json::Value;

use crate::state::{AttachedLayer, LambdaState, Layer, LayerVersion};

/// The parts of a layer ARN (`arn:<partition>:lambda:<region>:<account>:layer:<name>`)
/// or layer version ARN (the same plus `:<version>`).
///
/// Deliberately lenient about the region and account: fakecloud runs with
/// `--region local` and an arbitrary `--account-id`, and every layer ARN it
/// mints must parse back, or a function would silently lose its layers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayerArn {
    pub partition: String,
    pub region: String,
    pub account_id: String,
    pub layer_name: String,
    pub version: Option<i64>,
}

impl LayerArn {
    /// Parse a layer or layer version ARN. `None` when `arn` is not a Lambda
    /// layer ARN, a component is empty, or the version is not a positive
    /// integer.
    pub fn parse(arn: &str) -> Option<Self> {
        let parts: Vec<&str> = arn.split(':').collect();
        let (head, version) = match parts.len() {
            7 => (&parts[..], None),
            8 => {
                let v: i64 = parts[7].parse().ok().filter(|v| *v >= 1)?;
                (&parts[..7], Some(v))
            }
            _ => return None,
        };
        let [prefix, partition, service, region, account, kind, name] = head else {
            return None;
        };
        if *prefix != "arn" || *service != "lambda" || *kind != "layer" {
            return None;
        }
        if partition.is_empty() || region.is_empty() || account.is_empty() || name.is_empty() {
            return None;
        }
        Some(Self {
            partition: partition.to_string(),
            region: region.to_string(),
            account_id: account.to_string(),
            layer_name: name.to_string(),
            version,
        })
    }

    /// The unversioned layer ARN.
    pub fn layer_arn(&self) -> String {
        format!(
            "arn:{}:lambda:{}:{}:layer:{}",
            self.partition, self.region, self.account_id, self.layer_name
        )
    }

    /// The layer version ARN for `version`.
    pub fn version_arn(&self, version: i64) -> String {
        format!("{}:{version}", self.layer_arn())
    }
}

/// The layer a [`LayerArn`] names, if fakecloud holds one.
pub fn find_layer<'a>(
    accounts: &'a MultiRegionState<LambdaState>,
    arn: &LayerArn,
) -> Option<&'a Layer> {
    accounts
        .regional(&arn.account_id, &arn.region)?
        .layers
        .get(&arn.layer_name)
}

/// Version `version` of the layer `arn` names, in the account and region the
/// ARN names. The single lookup behind every layer version read: the layer
/// API, attachments, the invoke-time `/opt` mount and CloudFormation.
pub fn find_layer_version<'a>(
    accounts: &'a MultiRegionState<LambdaState>,
    arn: &LayerArn,
    version: i64,
) -> Option<(&'a Layer, &'a LayerVersion)> {
    let layer = find_layer(accounts, arn)?;
    let v = layer.versions.iter().find(|v| v.version == version)?;
    Some((layer, v))
}

/// Mutable [`find_layer_version`].
pub fn find_layer_version_mut<'a>(
    accounts: &'a mut MultiRegionState<LambdaState>,
    arn: &LayerArn,
    version: i64,
) -> Option<&'a mut LayerVersion> {
    accounts
        .regional_get_mut(&arn.account_id, &arn.region)?
        .layers
        .get_mut(&arn.layer_name)?
        .versions
        .iter_mut()
        .find(|v| v.version == version)
}

/// The layer version a version ARN string names (an attachment's `Arn`).
pub fn find_attached_version<'a>(
    accounts: &'a MultiRegionState<LambdaState>,
    version_arn: &str,
) -> Option<&'a LayerVersion> {
    let arn = LayerArn::parse(version_arn)?;
    find_layer_version(accounts, &arn, arn.version?).map(|(_, v)| v)
}

/// The current `RevisionId` of a layer version's policy, `None` without one.
/// A policy persisted before revisions were tracked gets one derived from
/// its content, so it stays stable until the policy changes.
pub fn policy_revision_id(v: &LayerVersion) -> Option<String> {
    let policy = v.policy.as_deref()?;
    Some(v.policy_revision_id.clone().unwrap_or_else(|| {
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(policy.as_bytes());
        let hex: String = digest.iter().take(16).map(|b| format!("{b:02x}")).collect();
        format!(
            "{}-{}-{}-{}-{}",
            &hex[..8],
            &hex[8..12],
            &hex[12..16],
            &hex[16..20],
            &hex[20..32]
        )
    }))
}

/// Who is asking for a layer: their account, identity ARN (for the error
/// message) and, resolved only when a policy asks for it, their
/// organization (for `aws:PrincipalOrgID` grants).
#[derive(Clone)]
pub struct LayerCaller {
    pub account_id: String,
    pub arn: String,
    orgs: Option<Arc<dyn ScpResolver>>,
    org_id: OnceLock<Option<String>>,
}

impl std::fmt::Debug for LayerCaller {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LayerCaller")
            .field("account_id", &self.account_id)
            .field("arn", &self.arn)
            .finish_non_exhaustive()
    }
}

impl LayerCaller {
    /// `account_id`, acting as `arn` (its root in `region`'s partition when
    /// `None`). `orgs` is the Organizations membership graph; the caller's
    /// organization is looked up there only when a foreign layer's policy
    /// carries an `aws:PrincipalOrgID` condition.
    pub fn for_account(
        account_id: &str,
        region: &str,
        arn: Option<String>,
        orgs: Option<Arc<dyn ScpResolver>>,
    ) -> Self {
        Self {
            account_id: account_id.to_string(),
            arn: arn.unwrap_or_else(|| {
                fakecloud_aws::arn::Arn::global_in(region, "iam", account_id, "root").to_string()
            }),
            orgs,
            org_id: OnceLock::new(),
        }
    }

    /// The caller of `req`.
    pub fn from_request(req: &AwsRequest, orgs: Option<Arc<dyn ScpResolver>>) -> Self {
        Self::for_account(
            &req.account_id,
            &req.region,
            req.principal.as_ref().map(|p| p.arn.clone()),
            orgs,
        )
    }

    /// The caller's organization, resolved on first use. This may run under
    /// the Lambda state lock; the Organizations state never takes the Lambda
    /// lock, so the two cannot deadlock.
    fn org_id(&self) -> Option<&str> {
        self.org_id
            .get_or_init(|| {
                self.orgs
                    .as_ref()
                    .and_then(|o| o.principal_org(&self.account_id))
                    .map(|(id, _)| id)
            })
            .as_deref()
    }
}

/// Lambda's answer to a caller who may not act on another account's layer
/// resource. Reads answer a missing version exactly like an unshared one.
pub fn access_denied(caller: &LayerCaller, action: &str, resource: &str) -> AwsServiceError {
    AwsServiceError::aws_error(
        StatusCode::FORBIDDEN,
        "AccessDeniedException",
        format!(
            "User: {} is not authorized to perform: lambda:{action} on resource: {resource} \
             because no resource-based policy allows the lambda:{action} action",
            caller.arn
        ),
    )
}

/// `InvalidParameterValueException` for attaching a version of one's own
/// layer that does not exist, worded as Lambda words it.
pub fn layer_version_does_not_exist(version_arn: &str) -> AwsServiceError {
    AwsServiceError::aws_error(
        StatusCode::BAD_REQUEST,
        "InvalidParameterValueException",
        format!("Layer version {version_arn} does not exist."),
    )
}

/// Whether a layer version policy lets `caller` perform `lambda:<action>`.
///
/// Account principals match by bare id or `arn:<partition>:iam::<id>:root`
/// in the layer's partition; `*` matches every account. The only condition
/// evaluated is `StringEquals` on `aws:PrincipalOrgID`. Anything that cannot
/// be evaluated (another condition, `NotPrincipal`, `NotAction`) fails
/// closed: such an `Allow` grants nothing and such a `Deny` applies.
/// `AddLayerVersionPermission` never writes those forms, so they can only
/// come from hand-edited persisted state. A `Deny` that applies wins.
pub fn policy_grants(policy: &str, action: &str, partition: &str, caller: &LayerCaller) -> bool {
    let Ok(doc) = serde_json::from_str::<Value>(policy) else {
        return false;
    };
    let statements: Vec<&Value> = match &doc["Statement"] {
        Value::Array(a) => a.iter().collect(),
        v @ Value::Object(_) => vec![v],
        _ => return false,
    };
    let wanted = format!("lambda:{action}");
    let mut allowed = false;
    for stmt in statements {
        let deny = match stmt["Effect"].as_str() {
            Some("Allow") => false,
            Some("Deny") => true,
            _ => continue,
        };
        // `Some(b)`: the statement's match is known; `None`: not evaluable.
        let outcome = statement_matches(stmt, &wanted, partition, caller);
        match (deny, outcome) {
            (true, Some(true) | None) => return false,
            (false, Some(true)) => allowed = true,
            _ => {}
        }
    }
    allowed
}

fn statement_matches(
    stmt: &Value,
    wanted: &str,
    partition: &str,
    caller: &LayerCaller,
) -> Option<bool> {
    if stmt.get("NotPrincipal").is_some() || stmt.get("NotAction").is_some() {
        return None;
    }
    if !matches_action(&stmt["Action"], wanted)
        || !matches_principal(&stmt["Principal"], partition, &caller.account_id)
    {
        return Some(false);
    }
    condition_outcome(&stmt["Condition"], caller)
}

fn string_values(v: &Value) -> Vec<&str> {
    match v {
        Value::String(s) => vec![s.as_str()],
        Value::Array(a) => a.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    }
}

fn matches_action(v: &Value, wanted: &str) -> bool {
    string_values(v)
        .into_iter()
        .any(|a| a == "*" || a.eq_ignore_ascii_case("lambda:*") || a.eq_ignore_ascii_case(wanted))
}

fn matches_principal(v: &Value, partition: &str, account: &str) -> bool {
    let candidates = match v {
        Value::Object(m) => m.get("AWS").map(string_values).unwrap_or_default(),
        other => string_values(other),
    };
    let root = format!("arn:{partition}:iam::{account}:root");
    candidates
        .into_iter()
        .any(|p| p == "*" || p == account || p == root)
}

/// `Some(true)` when every condition holds (or there is none), `Some(false)`
/// when an evaluable one does not, `None` when one cannot be evaluated.
fn condition_outcome(cond: &Value, caller: &LayerCaller) -> Option<bool> {
    if cond.is_null() {
        return Some(true);
    }
    let ops = cond.as_object()?;
    let mut holds = true;
    for (op, keys) in ops {
        let keys = keys.as_object()?;
        if op != "StringEquals" {
            return None;
        }
        for (key, expected) in keys {
            if !key.eq_ignore_ascii_case("aws:PrincipalOrgID") {
                return None;
            }
            let expected = string_values(expected);
            holds &= caller.org_id().is_some_and(|org| expected.contains(&org));
        }
    }
    Some(holds)
}

/// The layer version `arn`:`version`, as `caller` may see it for
/// `lambda:<action>`. The owner sees its own versions (`Ok(None)` when the
/// version does not exist); anyone else needs a policy grant, and gets
/// [`access_denied`] for a missing version exactly as for an unshared one.
pub fn authorized_layer_version<'a>(
    accounts: &'a MultiRegionState<LambdaState>,
    arn: &LayerArn,
    version: i64,
    caller: &LayerCaller,
    action: &str,
) -> Result<Option<(&'a Layer, &'a LayerVersion)>, AwsServiceError> {
    let found = find_layer_version(accounts, arn, version);
    if arn.account_id == caller.account_id {
        return Ok(found);
    }
    match found {
        Some((l, v))
            if v.policy
                .as_deref()
                .is_some_and(|p| policy_grants(p, action, &arn.partition, caller)) =>
        {
            Ok(Some((l, v)))
        }
        _ => Err(access_denied(caller, action, &arn.version_arn(version))),
    }
}

/// Resolve the `Layers` of a CreateFunction / UpdateFunctionConfiguration
/// (or a CloudFormation function) to attachments carrying each version's
/// `CodeSize`:
///
/// - a version of the caller's own layer must exist, else
///   [`layer_version_does_not_exist`];
/// - a version of another account's layer that fakecloud holds must be
///   shared with the caller (`lambda:GetLayerVersion`), else
///   [`access_denied`] (also for a missing version of that layer);
/// - another account's layer fakecloud holds no copy of at all (an
///   AWS-published or third-party layer such as Powertools) is attached as
///   given, with `CodeSize` 0 and nothing to mount, so templates that use
///   public layers keep deploying. Unlike the read operations, this lets a
///   caller tell an unknown foreign layer name from an unshared one;
/// - a value that is not a layer version ARN is attached as given, as
///   before.
pub fn resolve_layer_attachments(
    accounts: &MultiRegionState<LambdaState>,
    arns: Vec<String>,
    caller: &LayerCaller,
) -> Result<Vec<AttachedLayer>, AwsServiceError> {
    arns.into_iter()
        .map(|arn| {
            let code_size = match LayerArn::parse(&arn) {
                Some(
                    parsed @ LayerArn {
                        version: Some(version),
                        ..
                    },
                ) => {
                    if parsed.account_id == caller.account_id {
                        find_layer_version(accounts, &parsed, version)
                            .map(|(_, v)| v.code_size)
                            .ok_or_else(|| layer_version_does_not_exist(&arn))?
                    } else if find_layer(accounts, &parsed).is_some() {
                        authorized_layer_version(
                            accounts,
                            &parsed,
                            version,
                            caller,
                            "GetLayerVersion",
                        )?
                        .map(|(_, v)| v.code_size)
                        .unwrap_or(0)
                    } else {
                        0
                    }
                }
                _ => 0,
            };
            Ok(AttachedLayer { arn, code_size })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use serde_json::json;

    const OWNER: &str = "111111111111";
    const OTHER: &str = "222222222222";

    /// Puts every account in `org` (or in none), counting lookups.
    struct FixedOrg {
        org: Option<String>,
        lookups: std::sync::atomic::AtomicUsize,
    }

    impl ScpResolver for FixedOrg {
        fn scps_for(&self, _: &fakecloud_core::auth::Principal) -> Option<Vec<String>> {
            None
        }
        fn principal_org(&self, _account_id: &str) -> Option<(String, String)> {
            self.lookups
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.org
                .clone()
                .map(|o| (o.clone(), format!("{o}/r-root/")))
        }
    }

    fn fixed_org(org: Option<&str>) -> Arc<FixedOrg> {
        Arc::new(FixedOrg {
            org: org.map(str::to_string),
            lookups: Default::default(),
        })
    }

    fn caller(account: &str, org: Option<&str>) -> LayerCaller {
        LayerCaller::for_account(account, "us-east-1", None, Some(fixed_org(org)))
    }

    fn policy(stmts: Value) -> String {
        json!({"Version": "2012-10-17", "Statement": stmts}).to_string()
    }

    fn stmt(principal: Value) -> Value {
        json!({"Sid": "s", "Effect": "Allow", "Principal": principal,
               "Action": "lambda:GetLayerVersion", "Resource": "r"})
    }

    #[test]
    fn parses_layer_and_version_arns() {
        assert_eq!(
            LayerArn::parse("arn:aws:lambda:us-east-1:123456789012:layer:my-layer:3"),
            Some(LayerArn {
                partition: "aws".into(),
                region: "us-east-1".into(),
                account_id: "123456789012".into(),
                layer_name: "my-layer".into(),
                version: Some(3),
            })
        );
        let unversioned =
            LayerArn::parse("arn:aws-cn:lambda:cn-north-1:123456789012:layer:l").unwrap();
        assert_eq!(unversioned.version, None);
        assert_eq!(unversioned.partition, "aws-cn");
        assert_eq!(
            unversioned.version_arn(2),
            "arn:aws-cn:lambda:cn-north-1:123456789012:layer:l:2"
        );
    }

    /// fakecloud runs with `--region local` and any `--account-id`; the ARNs
    /// it mints there must parse back, or invoke would drop the layer.
    #[test]
    fn parses_every_arn_fakecloud_mints() {
        for (region, account) in [
            ("local", "000000000000"),
            ("us-east-1", "1"),
            ("my-region", "acct-x"),
            ("eu-west-1", "123456789012"),
            ("cn-north-1", "123456789012"),
        ] {
            let minted = crate::state::layer_arn(region, account, "lyr");
            let parsed = LayerArn::parse(&format!("{minted}:7")).expect(&minted);
            assert_eq!(parsed.region, region);
            assert_eq!(parsed.account_id, account);
            assert_eq!(parsed.layer_name, "lyr");
            assert_eq!(parsed.version, Some(7));
            assert_eq!(parsed.layer_arn(), minted);
            assert_eq!(LayerArn::parse(&minted).unwrap().version, None);
        }
    }

    #[test]
    fn rejects_non_layer_arns() {
        for bad in [
            "",
            "my-layer",
            "arn:aws:lambda:us-east-1:123456789012:function:f",
            "arn:aws:lambda:us-east-1:123456789012:function:f:1",
            "arn:aws:s3:us-east-1:123456789012:layer:l:1",
            "arn:aws:lambda::123456789012:layer:l:1",
            "arn:aws:lambda:us-east-1::layer:l:1",
            "arn:aws:lambda:us-east-1:123456789012:layer::1",
            "arn:aws:lambda:us-east-1:123456789012:layer:l:abc",
            "arn:aws:lambda:us-east-1:123456789012:layer:l:0",
            "arn:aws:lambda:us-east-1:123456789012:layer:l:-1",
            "arn:aws:lambda:us-east-1:123456789012:layer:l:1:2",
        ] {
            assert_eq!(LayerArn::parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn grants_match_account_root_and_wildcard_principals() {
        let other = caller(OTHER, None);
        for principal in [
            json!({"AWS": format!("arn:aws:iam::{OTHER}:root")}),
            json!({"AWS": ["arn:aws:iam::333333333333:root".to_string(), format!("arn:aws:iam::{OTHER}:root")]}),
            json!(OTHER),
            json!({"AWS": OTHER}),
            json!("*"),
            json!({"AWS": "*"}),
        ] {
            assert!(
                policy_grants(
                    &policy(json!([stmt(principal.clone())])),
                    "GetLayerVersion",
                    "aws",
                    &other
                ),
                "{principal}"
            );
        }
        for principal in [
            json!({"AWS": "arn:aws:iam::333333333333:root"}),
            json!("333333333333"),
            // A principal from another partition does not reach this layer.
            json!({"AWS": format!("arn:aws-cn:iam::{OTHER}:root")}),
        ] {
            assert!(
                !policy_grants(
                    &policy(json!([stmt(principal.clone())])),
                    "GetLayerVersion",
                    "aws",
                    &other
                ),
                "{principal}"
            );
        }
        // Partition-aware: the same root ARN in the layer's own partition.
        assert!(policy_grants(
            &policy(json!([stmt(
                json!({"AWS": format!("arn:aws-cn:iam::{OTHER}:root")})
            )])),
            "GetLayerVersion",
            "aws-cn",
            &other
        ));
    }

    #[test]
    fn grants_cover_only_the_granted_action() {
        let p = policy(json!([stmt(json!("*"))]));
        let other = caller(OTHER, None);
        assert!(policy_grants(&p, "GetLayerVersion", "aws", &other));
        assert!(!policy_grants(&p, "ListLayerVersions", "aws", &other));
        assert!(!policy_grants(&p, "DeleteLayerVersion", "aws", &other));
        assert!(!policy_grants("not json", "GetLayerVersion", "aws", &other));
        assert!(!policy_grants("{}", "GetLayerVersion", "aws", &other));
    }

    #[test]
    fn org_condition_admits_only_members_of_the_organization() {
        let mut s = stmt(json!("*"));
        s["Condition"] = json!({"StringEquals": {"aws:PrincipalOrgID": "o-abcdefghij"}});
        let p = policy(json!([s]));
        assert!(policy_grants(
            &p,
            "GetLayerVersion",
            "aws",
            &caller(OTHER, Some("o-abcdefghij"))
        ));
        assert!(!policy_grants(
            &p,
            "GetLayerVersion",
            "aws",
            &caller(OTHER, Some("o-zzzzzzzzzz"))
        ));
        assert!(!policy_grants(
            &p,
            "GetLayerVersion",
            "aws",
            &caller(OTHER, None)
        ));
        // An Allow whose condition cannot be evaluated grants nothing.
        for cond in [
            json!({"StringLike": {"aws:PrincipalOrgID": "o-*"}}),
            json!({"StringEquals": {"aws:SourceAccount": OTHER}}),
            json!("garbage"),
        ] {
            let mut odd = stmt(json!("*"));
            odd["Condition"] = cond.clone();
            assert!(
                !policy_grants(
                    &policy(json!([odd])),
                    "GetLayerVersion",
                    "aws",
                    &caller(OTHER, Some("o-abcdefghij"))
                ),
                "{cond}"
            );
        }
    }

    #[test]
    fn a_deny_that_cannot_be_evaluated_still_applies() {
        let allow = stmt(json!("*"));
        let mut deny = stmt(json!("*"));
        deny["Effect"] = json!("Deny");
        deny["Condition"] = json!({"StringLike": {"aws:PrincipalArn": "*"}});
        assert!(!policy_grants(
            &policy(json!([allow.clone(), deny])),
            "GetLayerVersion",
            "aws",
            &caller(OTHER, None)
        ));
        // A Deny on another action does not touch this one.
        let mut other_action = stmt(json!("*"));
        other_action["Effect"] = json!("Deny");
        other_action["Action"] = json!("lambda:DeleteLayerVersion");
        other_action["Condition"] = json!({"StringLike": {"aws:PrincipalArn": "*"}});
        assert!(policy_grants(
            &policy(json!([allow, other_action])),
            "GetLayerVersion",
            "aws",
            &caller(OTHER, None)
        ));
    }

    /// `AddLayerVersionPermission` never writes `NotPrincipal` / `NotAction`;
    /// if persisted state carries them anyway they fail closed.
    #[test]
    fn not_principal_and_not_action_fail_closed() {
        for key in ["NotPrincipal", "NotAction"] {
            let mut allow = json!({"Sid": "s", "Effect": "Allow", "Resource": "r"});
            allow[key] = json!(if key == "NotPrincipal" {
                json!({"AWS": "arn:aws:iam::333333333333:root"})
            } else {
                json!("lambda:DeleteLayerVersion")
            });
            if key == "NotPrincipal" {
                allow["Action"] = json!("lambda:GetLayerVersion");
            } else {
                allow["Principal"] = json!("*");
            }
            assert!(
                !policy_grants(
                    &policy(json!([allow.clone()])),
                    "GetLayerVersion",
                    "aws",
                    &caller(OTHER, None)
                ),
                "Allow with {key}"
            );
            let mut deny = allow;
            deny["Effect"] = json!("Deny");
            assert!(
                !policy_grants(
                    &policy(json!([stmt(json!("*")), deny])),
                    "GetLayerVersion",
                    "aws",
                    &caller(OTHER, None)
                ),
                "Deny with {key}"
            );
        }
    }

    /// The organization is looked up only for a policy that asks for it.
    #[test]
    fn the_organization_is_resolved_lazily() {
        let orgs = fixed_org(Some("o-abcdefghij"));
        let c = LayerCaller::for_account(OTHER, "us-east-1", None, Some(orgs.clone()));
        assert!(policy_grants(
            &policy(json!([stmt(json!(OTHER))])),
            "GetLayerVersion",
            "aws",
            &c
        ));
        assert_eq!(orgs.lookups.load(std::sync::atomic::Ordering::SeqCst), 0);
        let mut org_stmt = stmt(json!("*"));
        org_stmt["Condition"] = json!({"StringEquals": {"aws:PrincipalOrgID": "o-abcdefghij"}});
        let p = policy(json!([org_stmt]));
        assert!(policy_grants(&p, "GetLayerVersion", "aws", &c));
        assert!(policy_grants(&p, "GetLayerVersion", "aws", &c));
        assert_eq!(orgs.lookups.load(std::sync::atomic::Ordering::SeqCst), 1);
        // Attaching an own or unknown layer never consults Organizations.
        let orgs = fixed_org(Some("o-abcdefghij"));
        let owner = LayerCaller::for_account(OWNER, "us-east-1", None, Some(orgs.clone()));
        resolve_layer_attachments(
            &accounts_with(None),
            vec![
                version_arn(1),
                "arn:aws:lambda:us-east-1:017000801446:layer:P:1".into(),
            ],
            &owner,
        )
        .unwrap();
        assert_eq!(orgs.lookups.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[test]
    fn a_caller_acts_as_its_principal_or_account_root() {
        assert_eq!(
            LayerCaller::for_account(OTHER, "cn-north-1", None, None).arn,
            format!("arn:aws-cn:iam::{OTHER}:root")
        );
        assert_eq!(
            LayerCaller::for_account(
                OTHER,
                "us-east-1",
                Some("arn:aws:iam::x:user/u".into()),
                None
            )
            .arn,
            "arn:aws:iam::x:user/u"
        );
    }

    #[test]
    fn a_legacy_policy_gets_a_stable_revision() {
        let accounts = accounts_with(Some(policy(json!([stmt(json!("*"))]))));
        let (_, v) =
            find_layer_version(&accounts, &LayerArn::parse(&version_arn(1)).unwrap(), 1).unwrap();
        let rev = policy_revision_id(v).unwrap();
        assert_eq!(policy_revision_id(v).unwrap(), rev);
        let mut changed = v.clone();
        changed.policy = Some(policy(json!([stmt(json!(OTHER))])));
        assert_ne!(policy_revision_id(&changed).unwrap(), rev);
        changed.policy_revision_id = Some("stored".into());
        assert_eq!(policy_revision_id(&changed).as_deref(), Some("stored"));
        changed.policy = None;
        assert_eq!(policy_revision_id(&changed), None);
    }

    #[test]
    fn a_matching_deny_wins() {
        let mut deny = stmt(json!(OTHER));
        deny["Effect"] = json!("Deny");
        let p = policy(json!([stmt(json!("*")), deny]));
        assert!(!policy_grants(
            &p,
            "GetLayerVersion",
            "aws",
            &caller(OTHER, None)
        ));
        assert!(policy_grants(
            &p,
            "GetLayerVersion",
            "aws",
            &caller("333333333333", None)
        ));
    }

    fn accounts_with(policy: Option<String>) -> MultiRegionState<LambdaState> {
        let mut accounts: MultiRegionState<LambdaState> =
            fakecloud_core::multi_account::MultiAccountState::new(OWNER, "us-east-1", "");
        let layer_arn = crate::state::layer_arn("us-east-1", OWNER, "shared");
        let mut layer = Layer::new("shared", layer_arn.clone());
        layer.versions.push(LayerVersion {
            version: 1,
            layer_version_arn: format!("{layer_arn}:1"),
            description: String::new(),
            created_date: Utc::now(),
            compatible_runtimes: Vec::new(),
            license_info: String::new(),
            policy,
            policy_revision_id: None,
            code_zip: Some(b"zip".to_vec()),
            code_sha256: String::new(),
            code_size: 3,
            compatible_architectures: Vec::new(),
        });
        layer.last_version = 1;
        accounts
            .regional_mut(OWNER, "us-east-1")
            .layers
            .insert("shared".into(), layer);
        accounts
    }

    fn version_arn(v: i64) -> String {
        format!("arn:aws:lambda:us-east-1:{OWNER}:layer:shared:{v}")
    }

    #[test]
    fn foreign_missing_and_unshared_versions_answer_identically() {
        let accounts = accounts_with(None);
        let other = caller(OTHER, None);
        let lookup = |v: i64| {
            let arn = LayerArn::parse(&version_arn(v)).unwrap();
            authorized_layer_version(&accounts, &arn, v, &other, "GetLayerVersion")
                .map(|r| r.is_some())
                .unwrap_err()
        };
        let unshared = lookup(1);
        let missing = lookup(9);
        assert_eq!(unshared.code(), "AccessDeniedException");
        assert_eq!(missing.code(), "AccessDeniedException");
        assert_eq!(unshared.status(), missing.status());
        assert_eq!(
            unshared.message().replace(":1 ", ":N "),
            missing.message().replace(":9 ", ":N ")
        );
        // The owner sees its own versions, and a missing one as absent.
        let owner = caller(OWNER, None);
        let arn = LayerArn::parse(&version_arn(1)).unwrap();
        assert!(
            authorized_layer_version(&accounts, &arn, 1, &owner, "GetLayerVersion")
                .unwrap()
                .is_some()
        );
        assert!(
            authorized_layer_version(&accounts, &arn, 9, &owner, "GetLayerVersion")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn attaching_needs_a_grant_for_foreign_layers() {
        let shared = accounts_with(Some(policy(json!([stmt(json!(OTHER))]))));
        let attached =
            resolve_layer_attachments(&shared, vec![version_arn(1)], &caller(OTHER, None)).unwrap();
        assert_eq!(attached[0].code_size, 3);

        let unshared = accounts_with(None);
        for arn in [version_arn(1), version_arn(9)] {
            let err =
                resolve_layer_attachments(&unshared, vec![arn], &caller(OTHER, None)).unwrap_err();
            assert_eq!(err.code(), "AccessDeniedException");
        }
        // The owner attaches its own version without a grant, but not one
        // that does not exist.
        let own = resolve_layer_attachments(&unshared, vec![version_arn(1)], &caller(OWNER, None))
            .unwrap();
        assert_eq!(own[0].code_size, 3);
        for missing in [
            version_arn(9),
            format!("arn:aws:lambda:us-east-1:{OWNER}:layer:never-published:1"),
        ] {
            let err =
                resolve_layer_attachments(&unshared, vec![missing.clone()], &caller(OWNER, None))
                    .unwrap_err();
            assert_eq!(err.code(), "InvalidParameterValueException");
            assert_eq!(
                err.message(),
                format!("Layer version {missing} does not exist.")
            );
        }
        // A layer fakecloud holds no copy of is attached as given.
        let external = "arn:aws:lambda:us-east-1:017000801446:layer:Powertools:68".to_string();
        let ext =
            resolve_layer_attachments(&unshared, vec![external.clone()], &caller(OTHER, None))
                .unwrap();
        assert_eq!(ext[0].arn, external);
        assert_eq!(ext[0].code_size, 0);
    }

    #[test]
    fn find_layer_version_reads_the_arn_account_and_region() {
        let mut accounts = accounts_with(None);
        let arn = LayerArn::parse(&version_arn(1)).unwrap();
        assert_eq!(
            find_layer_version(&accounts, &arn, 1).map(|(_, v)| v.code_size),
            Some(3)
        );
        assert!(find_layer_version(&accounts, &arn, 2).is_none());
        let west =
            LayerArn::parse(&format!("arn:aws:lambda:eu-west-1:{OWNER}:layer:shared:1")).unwrap();
        assert!(find_layer_version(&accounts, &west, 1).is_none());
        assert!(find_layer_version_mut(&mut accounts, &west, 1).is_none());
        find_layer_version_mut(&mut accounts, &arn, 1)
            .unwrap()
            .code_size = 5;
        assert_eq!(
            find_attached_version(&accounts, &version_arn(1)).map(|v| v.code_size),
            Some(5)
        );
        // An attachment must name a version.
        assert!(find_attached_version(
            &accounts,
            &format!("arn:aws:lambda:us-east-1:{OWNER}:layer:shared")
        )
        .is_none());
    }
}
