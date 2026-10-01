//! In-memory state for Route 53 resources.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};

use crate::model::{HealthCheckConfig, HostedZoneFeatures, ResourceRecordSet, VPC};

pub type SharedRoute53State = Arc<RwLock<Route53Accounts>>;

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Route53Accounts {
    pub accounts: BTreeMap<String, AccountState>,
}

/// On-disk snapshot envelope for Route 53 state. Versioned so format changes
/// fail loudly on upgrade rather than silently mis-parsing.
#[derive(Clone, Serialize, Deserialize)]
pub struct Route53Snapshot {
    pub schema_version: u32,
    #[serde(default)]
    pub accounts: Option<Route53Accounts>,
}

///
/// v2: resources live under the account that created them. v1 kept every
/// resource in one [`LEGACY_ACCOUNT`] bucket whatever the caller's account.
pub const ROUTE53_SNAPSHOT_SCHEMA_VERSION: u32 = 2;

/// The single account bucket v1 snapshots stored every resource under.
const LEGACY_ACCOUNT: &str = "000000000000";

/// Route 53 resources CloudFormation stacks provisioned, by physical id,
/// mapped to the stack's account. A v1 snapshot stored them in the shared
/// legacy bucket, and Route 53 resources carry no account of their own (their
/// ARNs have none), so the owning stack's ARN is the only record of who owns
/// them. Built from the CloudFormation state when a v1 snapshot is loaded.
#[derive(Debug, Default, Clone)]
pub struct StackOwnedResources {
    hosted_zones: BTreeMap<String, String>,
    health_checks: BTreeMap<String, String>,
}

impl StackOwnedResources {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a stack resource. Only `AWS::Route53::HostedZone` and
    /// `AWS::Route53::HealthCheck` own top-level Route 53 state: record sets,
    /// DNSSEC and key-signing keys live inside (or are keyed by) their zone
    /// and follow it. The owner is the account in `stack_id`, the stack ARN.
    pub fn record(&mut self, stack_id: &str, resource_type: &str, physical_id: &str) {
        let Some(account) = arn_account(stack_id) else {
            return;
        };
        let map = match resource_type {
            "AWS::Route53::HostedZone" => &mut self.hosted_zones,
            "AWS::Route53::HealthCheck" => &mut self.health_checks,
            _ => return,
        };
        let id = physical_id.trim_start_matches("/hostedzone/");
        if !id.is_empty() {
            map.insert(id.to_string(), account.to_string());
        }
    }
}

/// The 12-digit account an ARN names, if any.
fn arn_account(arn: &str) -> Option<&str> {
    let account = arn.strip_prefix("arn:")?.split(':').nth(3)?;
    (account.len() == 12 && account.bytes().all(|b| b.is_ascii_digit())).then_some(account)
}

/// Parse a Route 53 snapshot, migrating older schema versions.
///
/// A v1 snapshot kept every resource in one shared [`LEGACY_ACCOUNT`] bucket.
/// Its stack-provisioned zones and health checks move to the stack's account
/// (`stack_owned`), with everything keyed by or scoped to such a zone following
/// it; every other entry was created through the API, which stored under the
/// legacy bucket whatever the caller, and moves to `default_account` (the
/// server's configured account).
///
/// `stack_owned` is only called for a v1 snapshot, so loading a current one
/// never walks the CloudFormation state.
pub fn parse_route53_snapshot(
    bytes: &[u8],
    default_account: &str,
    stack_owned: impl FnOnce() -> StackOwnedResources,
) -> Result<Route53Snapshot, serde_json::Error> {
    let mut snapshot: Route53Snapshot = serde_json::from_slice(bytes)?;
    if snapshot.schema_version < 2 {
        if let Some(accounts) = snapshot.accounts.as_mut() {
            accounts.migrate_legacy_bucket(default_account, &stack_owned());
        }
    }
    Ok(snapshot)
}

/// (De)serialize a `(A, B) -> V` map as a sequence of `(A, B, V)` triples. JSON
/// object keys must be strings, so tuple-keyed maps (traffic policies keyed by
/// `(id, version)`, KSKs / tags keyed by `(a, b)`) cannot be serialized
/// directly — without this, whole-state snapshot writes fail.
mod tuple2_map_serde {
    use std::collections::BTreeMap;

    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S, A, B, V>(
        map: &BTreeMap<(A, B), V>,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
        A: Serialize,
        B: Serialize,
        V: Serialize,
    {
        let entries: Vec<(&A, &B, &V)> = map.iter().map(|((a, b), v)| (a, b, v)).collect();
        entries.serialize(serializer)
    }

    pub fn deserialize<'de, D, A, B, V>(deserializer: D) -> Result<BTreeMap<(A, B), V>, D::Error>
    where
        D: Deserializer<'de>,
        A: Deserialize<'de> + Ord,
        B: Deserialize<'de> + Ord,
        V: Deserialize<'de>,
    {
        let entries: Vec<(A, B, V)> = Vec::deserialize(deserializer)?;
        Ok(entries.into_iter().map(|(a, b, v)| ((a, b), v)).collect())
    }
}

impl Route53Accounts {
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

    /// Split the v1 shared bucket by owner (see [`parse_route53_snapshot`]).
    /// Entries already stored under the owning account win over legacy ones
    /// with the same key.
    fn migrate_legacy_bucket(&mut self, default_account: &str, stack_owned: &StackOwnedResources) {
        let Some(legacy) = self.accounts.remove(LEGACY_ACCOUNT) else {
            return;
        };
        // Destructured exhaustively so a new field has to pick an owner here.
        let AccountState {
            hosted_zones,
            changes,
            health_checks,
            traffic_policies,
            traffic_policy_instances,
            dnssec_status,
            key_signing_keys,
            query_logging_configs,
            cidr_collections,
            reusable_delegation_sets,
            vpc_authorizations,
            cross_account_vpcs,
            vpc_authorization_consumers,
            tags,
        } = legacy;
        let owner = |map: &BTreeMap<String, String>, id: &str| -> String {
            map.get(id)
                .cloned()
                .unwrap_or_else(|| default_account.to_string())
        };
        let zone_owner = |id: &str| owner(&stack_owned.hosted_zones, id);
        let hc_owner = |id: &str| owner(&stack_owned.health_checks, id);
        // Make sure the default bucket exists even when the legacy one was
        // entirely stack-owned or empty.
        self.entry(default_account);

        for (id, zone) in hosted_zones {
            let account = zone_owner(&id);
            let bucket = self.entry(&account);
            if account != default_account && !zone.vpcs.is_empty() {
                // v1 kept no record of who associated a VPC, and every
                // account shared the zone, so the default account may have
                // associated any of them through the API. Keep its access to
                // disassociate and list them rather than lose it on load.
                let vpcs = zone
                    .vpcs
                    .iter()
                    .map(|v| (v.clone(), default_account.to_string()))
                    .collect();
                bucket.cross_account_vpcs.entry(id.clone()).or_insert(vpcs);
            }
            bucket.hosted_zones.entry(id).or_insert(zone);
        }
        for (id, hc) in health_checks {
            self.entry(&hc_owner(&id))
                .health_checks
                .entry(id)
                .or_insert(hc);
        }
        // A traffic policy instance writes its records into its zone, so it
        // follows the zone. Its policy keeps a single authoritative home: it
        // moves with its instances when every one of them lands in the same
        // account, and otherwise (no instances, or instances split across
        // accounts) stays in the default account.
        let mut policy_homes: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for (id, instance) in traffic_policy_instances {
            let account = zone_owner(&instance.hosted_zone_id);
            policy_homes
                .entry(instance.traffic_policy_id.clone())
                .or_default()
                .insert(account.clone());
            self.entry(&account)
                .traffic_policy_instances
                .entry(id)
                .or_insert(instance);
        }
        for (key, policy) in traffic_policies {
            let account = match policy_homes.get(&key.0) {
                Some(homes) if homes.len() == 1 => homes.iter().next().cloned(),
                _ => None,
            }
            .unwrap_or_else(|| default_account.to_string());
            self.entry(&account)
                .traffic_policies
                .entry(key)
                .or_insert(policy);
        }
        for (id, status) in dnssec_status {
            self.entry(&zone_owner(&id))
                .dnssec_status
                .entry(id)
                .or_insert(status);
        }
        for (key, ksk) in key_signing_keys {
            self.entry(&zone_owner(&key.0))
                .key_signing_keys
                .entry(key)
                .or_insert(ksk);
        }
        for (id, cfg) in query_logging_configs {
            self.entry(&zone_owner(&cfg.hosted_zone_id))
                .query_logging_configs
                .entry(id)
                .or_insert(cfg);
        }
        for (id, vpcs) in vpc_authorizations {
            self.entry(&zone_owner(&id))
                .vpc_authorizations
                .entry(id)
                .or_insert(vpcs);
        }
        for (id, consumers) in vpc_authorization_consumers {
            self.entry(&zone_owner(&id))
                .vpc_authorization_consumers
                .entry(id)
                .or_insert(consumers);
        }
        for (id, vpcs) in cross_account_vpcs {
            self.entry(&zone_owner(&id))
                .cross_account_vpcs
                .entry(id)
                .or_insert(vpcs);
        }
        for (key, value) in tags {
            let account = match key.0.as_str() {
                "hostedzone" => zone_owner(&key.1),
                "healthcheck" => hc_owner(&key.1),
                _ => default_account.to_string(),
            };
            self.entry(&account).tags.entry(key).or_insert(value);
        }
        // Account-level resources with no zone: always API-created. (A stack's
        // zone never references a reusable delegation set: the provisioner
        // creates zones without one.)
        let default = self.entry(default_account);
        for (id, change) in changes {
            default.changes.entry(id).or_insert(change);
        }
        for (id, collection) in cidr_collections {
            default.cidr_collections.entry(id).or_insert(collection);
        }
        for (id, set) in reusable_delegation_sets {
            default.reusable_delegation_sets.entry(id).or_insert(set);
        }
    }

    /// The account that owns hosted zone `zone_id`, if any. Hosted zone ids
    /// are unique across all accounts (see [`Self::unused_zone_id`]).
    pub fn zone_owner(&self, zone_id: &str) -> Option<&str> {
        self.accounts
            .iter()
            .find(|(_, a)| a.hosted_zones.contains_key(zone_id))
            .map(|(id, _)| id.as_str())
    }

    /// The state of the account that owns hosted zone `zone_id`, if any.
    pub fn zone_account(&self, zone_id: &str) -> Option<&AccountState> {
        self.accounts
            .values()
            .find(|a| a.hosted_zones.contains_key(zone_id))
    }

    /// Draw a hosted zone id no account holds.
    ///
    /// Route 53 zone ids are global: a VPC owner associates another account's
    /// private zone by id, and the DNS resolver and admin endpoints find zones
    /// by id without knowing the owner. Ids therefore stay unique across every
    /// account even though the zones themselves are per-account.
    pub fn unused_zone_id(&self) -> String {
        loop {
            let raw = uuid::Uuid::new_v4().simple().to_string().to_uppercase();
            let id = format!("Z{}", &raw[..14]);
            if self.zone_owner(&id).is_none() {
                return id;
            }
        }
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct AccountState {
    pub hosted_zones: BTreeMap<String, StoredHostedZone>,
    pub changes: BTreeMap<String, StoredChange>,
    pub health_checks: BTreeMap<String, StoredHealthCheck>,
    /// Keyed by `(traffic_policy_id, version)`. Each `CreateTrafficPolicyVersion`
    /// inserts a new entry alongside the existing versions.
    #[serde(with = "tuple2_map_serde")]
    pub traffic_policies: BTreeMap<(String, i64), StoredTrafficPolicy>,
    pub traffic_policy_instances: BTreeMap<String, StoredTrafficPolicyInstance>,
    /// Per-zone DNSSEC `ServeSignature` status (SIGNING / NOT_SIGNING). Absent
    /// entries are treated as NOT_SIGNING.
    pub dnssec_status: BTreeMap<String, String>,
    /// Keyed by `(hosted_zone_id, ksk_name)`.
    #[serde(with = "tuple2_map_serde")]
    pub key_signing_keys: BTreeMap<(String, String), StoredKeySigningKey>,
    pub query_logging_configs: BTreeMap<String, StoredQueryLoggingConfig>,
    pub cidr_collections: BTreeMap<String, StoredCidrCollection>,
    pub reusable_delegation_sets: BTreeMap<String, StoredReusableDelegationSet>,
    /// Per-zone authorized cross-account VPCs that may be associated next.
    pub vpc_authorizations: BTreeMap<String, Vec<VPC>>,
    /// Per-zone VPCs another account associated with this account's zone
    /// (cross-account `AssociateVPCWithHostedZone`), each paired with the
    /// associating (VPC-owning) account. That account may disassociate the
    /// VPC again and sees the zone in `ListHostedZonesByVPC`; no other
    /// account may.
    #[serde(default)]
    pub cross_account_vpcs: BTreeMap<String, Vec<(VPC, String)>>,
    /// Per-zone VPC authorizations already consumed by a cross-account
    /// association, each with the account that used it. When the VPC's owner
    /// is unknown (no EC2 VPC with that id), the first account to use an
    /// authorization is taken as the VPC's owner, and no other account may use
    /// it. Dropped with the authorization.
    #[serde(default)]
    pub vpc_authorization_consumers: BTreeMap<String, Vec<(VPC, String)>>,
    /// Tag bag keyed by `(resource_type, resource_id)`. Both supported
    /// resource types ("healthcheck", "hostedzone") share the bag; the
    /// resource-type discriminator is in the key tuple.
    #[serde(with = "tuple2_map_serde")]
    pub tags: BTreeMap<(String, String), BTreeMap<String, String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredHostedZone {
    pub id: String,
    pub name: String,
    pub caller_reference: String,
    pub comment: Option<String>,
    pub private_zone: bool,
    pub features: Option<HostedZoneFeatures>,
    pub vpcs: Vec<VPC>,
    pub delegation_set_id: Option<String>,
    pub name_servers: Vec<String>,
    pub created_time: DateTime<Utc>,
    pub resource_record_sets: Vec<ResourceRecordSet>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredChange {
    pub id: String,
    pub status: String,
    pub submitted_at: DateTime<Utc>,
    pub comment: Option<String>,
    /// Number of times GetChange has read this row. New changes start
    /// at PENDING; once a few reads have happened we flip to INSYNC to
    /// mirror real Route53's propagation delay without making tests
    /// wait wall-clock seconds.
    #[serde(default)]
    pub read_count: u32,
}

impl StoredChange {
    pub fn pending(id: String, submitted_at: DateTime<Utc>, comment: Option<String>) -> Self {
        Self {
            id,
            status: "PENDING".to_string(),
            submitted_at,
            comment,
            read_count: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum HealthCheckStatus {
    #[default]
    Success,
    Failure,
    /// The endpoint did not respond within the request timeout. Surfaced
    /// in `<Status>` as `"Failure: Connection timed out"` unless an
    /// explicit `last_failure_reason` is supplied.
    Timeout,
    /// Route 53 could not resolve the FQDN. Surfaced as
    /// `"Failure: DNS resolution failed"`.
    DnsError,
    /// Not enough recent observations to compute a definitive verdict.
    /// Surfaced as `"InsufficientDataPoints"` (no `Success`/`Failure`
    /// prefix, mirroring real AWS status strings).
    InsufficientDataPoints,
    /// Status could not be determined for any other reason. Surfaced as
    /// `"Unknown"`.
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredHealthCheck {
    pub id: String,
    pub caller_reference: String,
    pub version: i64,
    pub config: HealthCheckConfig,
    pub created_time: DateTime<Utc>,
    /// Status reported by `GetHealthCheckStatus`. Defaults to `Success`;
    /// flipped via the admin endpoint at
    /// `POST /_fakecloud/route53/health-checks/{id}/status` so callers
    /// can simulate failover scenarios in tests.
    #[serde(default)]
    pub status: HealthCheckStatus,
    /// Last failure reason returned by `GetHealthCheckLastFailureReason`
    /// and appended to the `Status` element when `status = Failure`.
    /// `None` when the check has never reported a failure.
    #[serde(default)]
    pub last_failure_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredTrafficPolicy {
    pub id: String,
    pub version: i64,
    pub name: String,
    pub policy_type: String,
    pub document: String,
    pub comment: Option<String>,
    pub created_time: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredTrafficPolicyInstance {
    pub id: String,
    pub hosted_zone_id: String,
    pub name: String,
    pub ttl: i64,
    pub state: String,
    pub message: String,
    pub traffic_policy_id: String,
    pub traffic_policy_version: i64,
    pub traffic_policy_type: String,
    pub created_time: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredKeySigningKey {
    pub hosted_zone_id: String,
    pub name: String,
    pub kms_arn: String,
    pub status: String,
    pub caller_reference: String,
    pub created_date: DateTime<Utc>,
    pub last_modified_date: DateTime<Utc>,
    pub key_tag: i32,
    /// PKCS#8 PEM-encoded ECDSA P-256 private key (algorithm 13 / RFC
    /// 6605). Generated deterministically from `(hosted_zone_id, name)`
    /// so persistence snapshots restore the same DNSKEY/RRSIGs across
    /// restarts. Not exposed to the AWS-facing XML — only used by the
    /// `/_fakecloud/route53/zones/{id}/dnssec/*` admin endpoints and the
    /// signed-RRset machinery surfaced through `TestDNSAnswer`.
    #[serde(default)]
    pub private_key_pem: String,
    /// SubjectPublicKeyInfo DER bytes for the matching public key.
    /// Stored alongside the private key so consumers can fetch the
    /// public half without re-deriving it on every read.
    #[serde(default)]
    pub public_key_der: Vec<u8>,
    /// DS record digest (SHA-256, hex) over the canonical DNSKEY RDATA
    /// for the parent zone to publish. Equivalent to the digest a
    /// real Route 53 returns alongside the KSK.
    #[serde(default)]
    pub ds_digest_hex: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredQueryLoggingConfig {
    pub id: String,
    pub hosted_zone_id: String,
    pub cloud_watch_logs_log_group_arn: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredCidrCollection {
    pub id: String,
    pub name: String,
    pub arn: String,
    pub version: i64,
    pub caller_reference: String,
    /// Maps location name -> sorted list of CIDR blocks.
    pub locations: BTreeMap<String, Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredReusableDelegationSet {
    pub id: String,
    pub caller_reference: String,
    pub name_servers: Vec<String>,
}

#[cfg(test)]
mod snapshot_migration_tests {
    use super::*;

    const STACK_ACCOUNT: &str = "222222222222";
    const DEFAULT: &str = "123456789012";

    fn zone(id: &str) -> StoredHostedZone {
        crate::build_hosted_zone(
            id,
            "example.com",
            format!("ref-{id}"),
            None,
            false,
            vec![],
            None,
        )
    }

    fn health_check(id: &str) -> StoredHealthCheck {
        StoredHealthCheck {
            id: id.to_string(),
            caller_reference: format!("ref-{id}"),
            version: 1,
            config: HealthCheckConfig {
                health_check_type: "HTTP".to_string(),
                ..Default::default()
            },
            created_time: Utc::now(),
            status: HealthCheckStatus::Success,
            last_failure_reason: None,
        }
    }

    /// A v1 snapshot: an API-created zone (`ZAPI`) and health check, plus a
    /// stack-created zone (`ZSTACK`, with a record set, DNSSEC, a query
    /// logging config, a VPC authorization and tags) and health check, all in
    /// the shared legacy bucket.
    fn v1_snapshot_bytes() -> Vec<u8> {
        let mut accounts = Route53Accounts::new();
        let legacy = accounts.entry(LEGACY_ACCOUNT);
        legacy.hosted_zones.insert("ZAPI".into(), zone("ZAPI"));
        let mut stack_zone = zone("ZSTACK");
        stack_zone.vpcs.push(VPC {
            vpc_id: Some("vpc-1".into()),
            vpc_region: Some("us-east-1".into()),
        });
        stack_zone
            .resource_record_sets
            .push(crate::model::ResourceRecordSet {
                name: "www.example.com.".to_string(),
                record_type: "A".to_string(),
                ttl: Some(60),
                ..Default::default()
            });
        legacy.hosted_zones.insert("ZSTACK".into(), stack_zone);
        legacy
            .health_checks
            .insert("hc-api".into(), health_check("hc-api"));
        legacy
            .health_checks
            .insert("hc-stack".into(), health_check("hc-stack"));
        legacy
            .dnssec_status
            .insert("ZSTACK".into(), "SIGNING".into());
        legacy.query_logging_configs.insert(
            "qlc-1".into(),
            StoredQueryLoggingConfig {
                id: "qlc-1".into(),
                hosted_zone_id: "ZSTACK".into(),
                cloud_watch_logs_log_group_arn: format!(
                    "arn:aws:logs:us-east-1:{STACK_ACCOUNT}:log-group:/aws/route53/example.com"
                ),
            },
        );
        legacy.vpc_authorizations.insert("ZSTACK".into(), vec![]);
        legacy.tags.insert(
            ("hostedzone".into(), "ZSTACK".into()),
            BTreeMap::from([("team".into(), "dns".into())]),
        );
        legacy
            .tags
            .insert(("healthcheck".into(), "hc-stack".into()), BTreeMap::new());
        legacy
            .tags
            .insert(("hostedzone".into(), "ZAPI".into()), BTreeMap::new());
        legacy.traffic_policies.insert(
            ("tp-1".into(), 1),
            StoredTrafficPolicy {
                id: "tp-1".into(),
                version: 1,
                name: "p".into(),
                policy_type: "A".into(),
                document: "{}".into(),
                comment: None,
                created_time: Utc::now(),
            },
        );
        legacy.traffic_policy_instances.insert(
            "tpi-1".into(),
            StoredTrafficPolicyInstance {
                id: "tpi-1".into(),
                hosted_zone_id: "ZSTACK".into(),
                name: "www.example.com.".into(),
                ttl: 60,
                state: "Applied".into(),
                message: String::new(),
                traffic_policy_id: "tp-1".into(),
                traffic_policy_version: 1,
                traffic_policy_type: "A".into(),
                created_time: Utc::now(),
            },
        );
        legacy.changes.insert(
            "C1".into(),
            StoredChange::pending("C1".into(), Utc::now(), None),
        );
        serde_json::to_vec(&Route53Snapshot {
            schema_version: 1,
            accounts: Some(accounts),
        })
        .unwrap()
    }

    fn stack_owned() -> StackOwnedResources {
        let stack = format!("arn:aws:cloudformation:us-east-1:{STACK_ACCOUNT}:stack/dns/abc");
        let mut owned = StackOwnedResources::new();
        owned.record(&stack, "AWS::Route53::HostedZone", "ZSTACK");
        owned.record(&stack, "AWS::Route53::HealthCheck", "hc-stack");
        owned.record(
            &stack,
            "AWS::Route53::RecordSet",
            "ZSTACK|www.example.com.|A",
        );
        owned.record(&stack, "AWS::SQS::Queue", "ZAPI");
        owned
    }

    #[test]
    fn a_v1_snapshot_moves_stack_created_resources_to_the_stacks_account() {
        let parsed = parse_route53_snapshot(&v1_snapshot_bytes(), DEFAULT, stack_owned)
            .expect("v1 snapshot loads");
        let accounts = parsed.accounts.unwrap();
        assert!(
            accounts.get(LEGACY_ACCOUNT).is_none(),
            "legacy bucket split"
        );

        let stack = accounts.get(STACK_ACCOUNT).expect("stack account bucket");
        let zone = &stack.hosted_zones["ZSTACK"];
        assert!(
            zone.resource_record_sets
                .iter()
                .any(|r| r.name == "www.example.com." && r.record_type == "A"),
            "record sets travel inside their zone"
        );
        assert!(stack.health_checks.contains_key("hc-stack"));
        assert_eq!(stack.dnssec_status["ZSTACK"], "SIGNING");
        assert!(stack.query_logging_configs.contains_key("qlc-1"));
        assert!(stack.vpc_authorizations.contains_key("ZSTACK"));
        // The default account keeps access to the VPCs it may have associated.
        assert_eq!(stack.cross_account_vpcs["ZSTACK"][0].1, DEFAULT);
        assert_eq!(
            stack.tags[&("hostedzone".to_string(), "ZSTACK".to_string())]["team"],
            "dns"
        );
        assert!(stack
            .tags
            .contains_key(&("healthcheck".to_string(), "hc-stack".to_string())));
        assert!(!stack.hosted_zones.contains_key("ZAPI"));
        // A traffic policy instance follows its zone, and its policy (all of
        // whose instances moved) follows it.
        assert!(stack.traffic_policy_instances.contains_key("tpi-1"));
        assert!(stack
            .traffic_policies
            .contains_key(&("tp-1".to_string(), 1)));

        // Everything else was API-created and belongs to the default account.
        let default = accounts.get(DEFAULT).expect("default account bucket");
        assert!(default.hosted_zones.contains_key("ZAPI"));
        assert!(default.health_checks.contains_key("hc-api"));
        assert!(default.changes.contains_key("C1"));
        assert!(
            !default
                .traffic_policies
                .contains_key(&("tp-1".to_string(), 1)),
            "the policy moved with its only instance, not copied"
        );
        assert!(default
            .tags
            .contains_key(&("hostedzone".to_string(), "ZAPI".to_string())));
        assert!(!default.hosted_zones.contains_key("ZSTACK"));
        assert!(!default.health_checks.contains_key("hc-stack"));
        assert_eq!(accounts.account_count(), 2);
    }

    #[test]
    fn a_v1_snapshot_keeps_api_resources_put_when_the_default_account_is_the_legacy_one() {
        let parsed =
            parse_route53_snapshot(&v1_snapshot_bytes(), LEGACY_ACCOUNT, stack_owned).unwrap();
        let accounts = parsed.accounts.unwrap();
        let legacy = accounts.get(LEGACY_ACCOUNT).unwrap();
        assert!(legacy.hosted_zones.contains_key("ZAPI"));
        assert!(!legacy.hosted_zones.contains_key("ZSTACK"));
        assert!(accounts
            .get(STACK_ACCOUNT)
            .unwrap()
            .hosted_zones
            .contains_key("ZSTACK"));
    }

    #[test]
    fn a_v1_snapshot_merges_into_an_existing_owner_bucket() {
        let mut value: serde_json::Value = serde_json::from_slice(&v1_snapshot_bytes()).unwrap();
        let existing = serde_json::to_value(AccountState {
            hosted_zones: BTreeMap::from([("ZOWN".to_string(), zone("ZOWN"))]),
            ..Default::default()
        })
        .unwrap();
        value["accounts"]["accounts"][STACK_ACCOUNT] = existing;
        let parsed =
            parse_route53_snapshot(&serde_json::to_vec(&value).unwrap(), DEFAULT, stack_owned)
                .unwrap();
        let accounts = parsed.accounts.unwrap();
        let stack = accounts.get(STACK_ACCOUNT).unwrap();
        let ids: Vec<&String> = stack.hosted_zones.keys().collect();
        assert_eq!(ids, ["ZOWN", "ZSTACK"]);
    }

    #[test]
    fn a_traffic_policy_whose_instances_split_across_accounts_stays_in_the_default_account() {
        let mut value: serde_json::Value = serde_json::from_slice(&v1_snapshot_bytes()).unwrap();
        // A second instance of tp-1, on the API-created zone.
        let mut second = value["accounts"]["accounts"][LEGACY_ACCOUNT]["traffic_policy_instances"]
            ["tpi-1"]
            .clone();
        second["id"] = "tpi-2".into();
        second["hosted_zone_id"] = "ZAPI".into();
        value["accounts"]["accounts"][LEGACY_ACCOUNT]["traffic_policy_instances"]["tpi-2"] = second;
        let parsed =
            parse_route53_snapshot(&serde_json::to_vec(&value).unwrap(), DEFAULT, stack_owned)
                .unwrap();
        let accounts = parsed.accounts.unwrap();
        let key = ("tp-1".to_string(), 1);
        let default = accounts.get(DEFAULT).unwrap();
        assert!(default.traffic_policies.contains_key(&key));
        assert!(default.traffic_policy_instances.contains_key("tpi-2"));
        let stack = accounts.get(STACK_ACCOUNT).unwrap();
        assert!(stack.traffic_policy_instances.contains_key("tpi-1"));
        assert!(
            !stack.traffic_policies.contains_key(&key),
            "one authoritative copy"
        );
    }

    #[test]
    fn a_current_snapshot_is_not_migrated() {
        let mut accounts = Route53Accounts::new();
        accounts
            .entry(LEGACY_ACCOUNT)
            .hosted_zones
            .insert("ZSTACK".into(), zone("ZSTACK"));
        let bytes = serde_json::to_vec(&Route53Snapshot {
            schema_version: ROUTE53_SNAPSHOT_SCHEMA_VERSION,
            accounts: Some(accounts),
        })
        .unwrap();
        let parsed = parse_route53_snapshot(&bytes, DEFAULT, || {
            panic!("a current snapshot must not build the stack-owned map")
        })
        .unwrap();
        let accounts = parsed.accounts.unwrap();
        assert!(accounts
            .get(LEGACY_ACCOUNT)
            .unwrap()
            .hosted_zones
            .contains_key("ZSTACK"));
        assert!(accounts.get(STACK_ACCOUNT).is_none());
    }

    #[test]
    fn stack_owned_resources_ignore_malformed_stack_arns() {
        let mut owned = StackOwnedResources::new();
        owned.record("not-an-arn", "AWS::Route53::HostedZone", "Z1");
        owned.record(
            "arn:aws:cloudformation:us-east-1:12:stack/x/y",
            "AWS::Route53::HostedZone",
            "Z2",
        );
        assert!(owned.hosted_zones.is_empty());
    }

    #[test]
    fn unused_zone_id_is_unique_across_accounts() {
        let mut accounts = Route53Accounts::new();
        accounts
            .entry("111111111111")
            .hosted_zones
            .insert("ZTAKEN".into(), zone("ZTAKEN"));
        let id = accounts.unused_zone_id();
        assert!(id.starts_with('Z') && id.len() == 15, "{id}");
        assert_eq!(accounts.zone_owner("ZTAKEN"), Some("111111111111"));
        assert!(accounts.zone_owner(&id).is_none());
    }
}
