//! In-memory state for CloudFront resources.

use std::collections::BTreeMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};

use crate::cfunctions::StoredConnectionFunction;
use crate::extras::{StoredAnycastIpList, StoredResourcePolicy, StoredTrustStore, StoredVpcOrigin};
use crate::extras2::StoredConnectionGroup;
use crate::fle::{
    StoredFieldLevelEncryption, StoredFieldLevelEncryptionProfile, StoredRealtimeLogConfig,
};
use crate::functions::{
    StoredFunction, StoredKeyGroup, StoredKeyValueStore, StoredMonitoringSubscription,
    StoredOriginAccessIdentity, StoredPublicKey,
};
use crate::model::{DistributionConfig, InvalidationBatch};
use crate::policies::{
    StoredCachePolicy, StoredContinuousDeploymentPolicy, StoredOriginAccessControl,
    StoredOriginRequestPolicy, StoredResponseHeadersPolicy,
};
use crate::streaming::StoredStreamingDistribution;
use crate::tenants::{StoredDistributionTenant, StoredTenantInvalidation};

pub type SharedCloudFrontState = Arc<RwLock<CloudFrontAccounts>>;

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct CloudFrontAccounts {
    pub accounts: BTreeMap<String, AccountState>,
}

/// On-disk snapshot envelope for CloudFront state. Versioned so format changes
/// fail loudly on upgrade rather than silently mis-parsing.
#[derive(Clone, Serialize, Deserialize)]
pub struct CloudFrontSnapshot {
    pub schema_version: u32,
    #[serde(default)]
    pub accounts: Option<CloudFrontAccounts>,
}

/// v2: distribution config members AWS spells with an upper-case acronym
/// (`MinTTL`, `IsIPV6Enabled`, `ACMCertificateArn`, ...) are stored under that
/// spelling; v1 wrote the PascalCase form (`MinTtl`, ...).
pub const CLOUDFRONT_SNAPSHOT_SCHEMA_VERSION: u32 = 2;

/// Distribution config member names v1 snapshots wrote, paired with the AWS
/// spelling v2 uses.
const V1_DISTRIBUTION_CONFIG_RENAMES: &[(&str, &str)] = &[
    ("MinTtl", "MinTTL"),
    ("DefaultTtl", "DefaultTTL"),
    ("MaxTtl", "MaxTTL"),
    ("ErrorCachingMinTtl", "ErrorCachingMinTTL"),
    ("IsIpv6Enabled", "IsIPV6Enabled"),
    ("IamCertificateId", "IAMCertificateId"),
    ("AcmCertificateArn", "ACMCertificateArn"),
    ("SslSupportMethod", "SSLSupportMethod"),
    ("FunctionArn", "FunctionARN"),
    ("LambdaFunctionArn", "LambdaFunctionARN"),
];

/// Parse an on-disk CloudFront snapshot, migrating older schema versions.
///
/// The migration rewrites the stored JSON rather than teaching the model
/// structs the old names, so the old spellings never become accepted on the
/// XML wire.
pub fn parse_cloudfront_snapshot(bytes: &[u8]) -> Result<CloudFrontSnapshot, serde_json::Error> {
    let mut value: serde_json::Value = serde_json::from_slice(bytes)?;
    let version = value
        .get("schema_version")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    if version < 2 {
        migrate_v1_distribution_configs(&mut value);
    }
    serde_json::from_value(value)
}

fn migrate_v1_distribution_configs(snapshot: &mut serde_json::Value) {
    let Some(accounts) = snapshot
        .pointer_mut("/accounts/accounts")
        .and_then(serde_json::Value::as_object_mut)
    else {
        return;
    };
    for account in accounts.values_mut() {
        let Some(distributions) = account
            .get_mut("distributions")
            .and_then(serde_json::Value::as_object_mut)
        else {
            continue;
        };
        for distribution in distributions.values_mut() {
            if let Some(config) = distribution.get_mut("config") {
                rename_keys(config);
            }
        }
    }
}

fn rename_keys(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            for (old, new) in V1_DISTRIBUTION_CONFIG_RENAMES {
                if let Some(v) = map.remove(*old) {
                    map.insert((*new).to_string(), v);
                }
            }
            map.values_mut().for_each(rename_keys);
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(rename_keys),
        _ => {}
    }
}

impl CloudFrontAccounts {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn account_count(&self) -> usize {
        self.accounts.len()
    }

    pub fn entry(&mut self, account_id: &str) -> &mut AccountState {
        self.accounts.entry(account_id.to_string()).or_default()
    }

    pub fn get(&self, account_id: &str) -> Option<&AccountState> {
        self.accounts.get(account_id)
    }

    /// Iterate every stored distribution across all accounts, paired with the
    /// owning account id. Used by the `/_fakecloud/cloudfront/distributions`
    /// introspection route (and, later, the data-plane supervisor).
    pub fn all_distributions(&self) -> impl Iterator<Item = (&String, &StoredDistribution)> {
        self.accounts.iter().flat_map(|(account_id, state)| {
            state.distributions.values().map(move |d| (account_id, d))
        })
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct AccountState {
    pub distributions: BTreeMap<String, StoredDistribution>,
    pub invalidations: BTreeMap<String, StoredInvalidation>,
    /// Tags keyed by ARN.
    pub tags: BTreeMap<String, Vec<Tag>>,
    pub origin_access_controls: BTreeMap<String, StoredOriginAccessControl>,
    pub cache_policies: BTreeMap<String, StoredCachePolicy>,
    pub origin_request_policies: BTreeMap<String, StoredOriginRequestPolicy>,
    pub response_headers_policies: BTreeMap<String, StoredResponseHeadersPolicy>,
    pub continuous_deployment_policies: BTreeMap<String, StoredContinuousDeploymentPolicy>,
    pub functions: BTreeMap<String, StoredFunction>,
    pub public_keys: BTreeMap<String, StoredPublicKey>,
    pub key_groups: BTreeMap<String, StoredKeyGroup>,
    pub key_value_stores: BTreeMap<String, StoredKeyValueStore>,
    pub origin_access_identities: BTreeMap<String, StoredOriginAccessIdentity>,
    /// Per-distribution monitoring subscription, keyed by distribution id.
    pub monitoring_subscriptions: BTreeMap<String, StoredMonitoringSubscription>,
    pub streaming_distributions: BTreeMap<String, StoredStreamingDistribution>,
    pub field_level_encryptions: BTreeMap<String, StoredFieldLevelEncryption>,
    pub field_level_encryption_profiles: BTreeMap<String, StoredFieldLevelEncryptionProfile>,
    /// Realtime log configs keyed by ARN.
    pub realtime_log_configs: BTreeMap<String, StoredRealtimeLogConfig>,
    pub vpc_origins: BTreeMap<String, StoredVpcOrigin>,
    pub anycast_ip_lists: BTreeMap<String, StoredAnycastIpList>,
    pub trust_stores: BTreeMap<String, StoredTrustStore>,
    /// Resource policies keyed by resource ARN.
    pub resource_policies: BTreeMap<String, StoredResourcePolicy>,
    pub connection_groups: BTreeMap<String, StoredConnectionGroup>,
    pub distribution_tenants: BTreeMap<String, StoredDistributionTenant>,
    pub tenant_invalidations: BTreeMap<String, StoredTenantInvalidation>,
    pub connection_functions: BTreeMap<String, StoredConnectionFunction>,
}

impl CloudFrontAccounts {
    /// Pre-seed the AWS-managed Cache, Origin Request, and Response
    /// Headers policies into the default account so callers that look
    /// them up by their well-known IDs (Terraform, CDK) get the same
    /// shape they get against AWS. The IDs and names mirror the AWS
    /// console output verbatim — the easiest way to keep tests source
    /// of truth.
    pub fn seed_managed_policies(&mut self, account_id: &str) {
        let account = self.entry(account_id);
        crate::policies::seed_managed(account);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredDistribution {
    pub id: String,
    pub arn: String,
    pub status: String,
    pub last_modified_time: DateTime<Utc>,
    pub domain_name: String,
    pub in_progress_invalidation_batches: u32,
    pub etag: String,
    pub config: DistributionConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredInvalidation {
    pub id: String,
    pub distribution_id: String,
    pub status: String,
    pub create_time: DateTime<Utc>,
    pub batch: InvalidationBatch,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Tag {
    pub key: String,
    pub value: Option<String>,
}

#[cfg(test)]
mod snapshot_migration_tests {
    use super::*;

    /// A config exercising every member v2 renamed.
    fn config_with_renamed_members() -> serde_json::Value {
        serde_json::json!({
            "CallerReference": "ref",
            "Origins": {"Quantity": 0},
            "DefaultCacheBehavior": {
                "TargetOriginId": "o1",
                "ViewerProtocolPolicy": "allow-all",
                "MinTtl": 1, "DefaultTtl": 2, "MaxTtl": 3,
                "FunctionAssociations": {"Quantity": 1, "Items": {"FunctionAssociation": [
                    {"FunctionArn": "arn:f", "EventType": "viewer-request"}]}},
                "LambdaFunctionAssociations": {"Quantity": 1, "Items": {"LambdaFunctionAssociation": [
                    {"LambdaFunctionArn": "arn:l", "EventType": "origin-request"}]}}
            },
            "CustomErrorResponses": {"Quantity": 1, "Items": {"CustomErrorResponse": [
                {"ErrorCode": 404, "ErrorCachingMinTtl": 10}]}},
            "Comment": "",
            "Enabled": true,
            "ViewerCertificate": {"IamCertificateId": "iam-1", "AcmCertificateArn": "arn:c", "SslSupportMethod": "sni-only"},
            "IsIpv6Enabled": false
        })
    }

    /// A v1 snapshot: a current one with the distribution config swapped for
    /// one written under the PascalCase names.
    fn v1_snapshot_bytes() -> Vec<u8> {
        let mut accounts = CloudFrontAccounts::new();
        accounts.entry("000000000000").distributions.insert(
            "E1".to_string(),
            StoredDistribution {
                id: "E1".to_string(),
                arn: "arn".to_string(),
                status: "Deployed".to_string(),
                last_modified_time: Utc::now(),
                domain_name: "e1.cloudfront.net".to_string(),
                in_progress_invalidation_batches: 0,
                etag: "T".to_string(),
                config: DistributionConfig::default(),
            },
        );
        let mut value = serde_json::to_value(CloudFrontSnapshot {
            schema_version: 1,
            accounts: Some(accounts),
        })
        .unwrap();
        *value
            .pointer_mut("/accounts/accounts/000000000000/distributions/E1/config")
            .unwrap() = config_with_renamed_members();
        serde_json::to_vec(&value).unwrap()
    }

    #[test]
    fn a_v1_snapshot_loads_with_its_pascal_case_member_names_migrated() {
        let snapshot = parse_cloudfront_snapshot(&v1_snapshot_bytes()).expect("v1 snapshot loads");
        let accounts = snapshot.accounts.unwrap();
        let config = &accounts.get("000000000000").unwrap().distributions["E1"].config;
        let dcb = &config.default_cache_behavior;
        assert_eq!(
            (dcb.min_ttl, dcb.default_ttl, dcb.max_ttl),
            (Some(1), Some(2), Some(3))
        );
        let fa = dcb.function_associations.as_ref().unwrap();
        assert_eq!(
            fa.items.as_ref().unwrap().function_association[0].function_arn,
            "arn:f"
        );
        let la = dcb.lambda_function_associations.as_ref().unwrap();
        assert_eq!(
            la.items.as_ref().unwrap().lambda_function_association[0].lambda_function_arn,
            "arn:l"
        );
        let rules = config.custom_error_responses.as_ref().unwrap();
        assert_eq!(
            rules.items.as_ref().unwrap().custom_error_response[0].error_caching_min_ttl,
            Some(10)
        );
        let vc = config.viewer_certificate.as_ref().unwrap();
        assert_eq!(vc.iam_certificate_id.as_deref(), Some("iam-1"));
        assert_eq!(vc.acm_certificate_arn.as_deref(), Some("arn:c"));
        assert_eq!(vc.ssl_support_method.as_deref(), Some("sni-only"));
        assert_eq!(config.is_ipv6_enabled, Some(false));
    }

    #[test]
    fn the_migration_only_runs_for_v1_snapshots() {
        // A v2 snapshot is read as written: the PascalCase names are not
        // CloudFront member names, so the required `FunctionARN` is missing.
        let mut value: serde_json::Value = serde_json::from_slice(&v1_snapshot_bytes()).unwrap();
        value["schema_version"] = serde_json::json!(2);
        let err = parse_cloudfront_snapshot(&serde_json::to_vec(&value).unwrap())
            .err()
            .expect("not migrated");
        assert!(err.to_string().contains("FunctionARN"), "{err}");
    }

    #[test]
    fn a_current_snapshot_round_trips_through_the_parser() {
        let mut accounts = CloudFrontAccounts::new();
        let mut config = DistributionConfig::default();
        config.default_cache_behavior.min_ttl = Some(7);
        accounts.entry("000000000000").distributions.insert(
            "E2".to_string(),
            StoredDistribution {
                id: "E2".to_string(),
                arn: "arn".to_string(),
                status: "Deployed".to_string(),
                last_modified_time: Utc::now(),
                domain_name: "e2.cloudfront.net".to_string(),
                in_progress_invalidation_batches: 0,
                etag: "T".to_string(),
                config,
            },
        );
        let bytes = serde_json::to_vec(&CloudFrontSnapshot {
            schema_version: CLOUDFRONT_SNAPSHOT_SCHEMA_VERSION,
            accounts: Some(accounts),
        })
        .unwrap();
        let parsed = parse_cloudfront_snapshot(&bytes).unwrap();
        let accounts = parsed.accounts.unwrap();
        let dist = &accounts.get("000000000000").unwrap().distributions["E2"];
        assert_eq!(dist.config.default_cache_behavior.min_ttl, Some(7));
    }
}
