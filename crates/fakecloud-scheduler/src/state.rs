use chrono::{DateTime, Utc};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::Arc;

use fakecloud_aws::arn::Arn;
use fakecloud_core::multi_account::{AccountState, MultiAccountState};

/// Default schedule group name, auto-created for every account and
/// never deletable (matches AWS behavior).
pub const DEFAULT_GROUP: &str = "default";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Schedule {
    pub arn: String,
    pub name: String,
    pub group_name: String,
    pub schedule_expression: String,
    pub schedule_expression_timezone: Option<String>,
    pub start_date: Option<DateTime<Utc>>,
    pub end_date: Option<DateTime<Utc>>,
    pub description: Option<String>,
    pub state: String, // ENABLED | DISABLED
    pub kms_key_arn: Option<String>,
    pub action_after_completion: String, // NONE | DELETE
    pub flexible_time_window: FlexibleTimeWindow,
    pub target: Target,
    pub creation_date: DateTime<Utc>,
    pub last_modification_date: DateTime<Utc>,
    /// Internal: wall-clock timestamp of the most recent fire. Unused in
    /// Batch 1 CRUD, consumed by the ticker in Batch 2.
    #[serde(default)]
    pub last_fired: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlexibleTimeWindow {
    pub mode: String, // OFF | FLEXIBLE
    pub maximum_window_in_minutes: Option<i64>,
}

impl Default for FlexibleTimeWindow {
    fn default() -> Self {
        Self {
            mode: "OFF".to_string(),
            maximum_window_in_minutes: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Target {
    pub arn: String,
    pub role_arn: String,
    pub input: Option<String>,
    pub dead_letter_config: Option<DeadLetterConfig>,
    pub retry_policy: Option<RetryPolicy>,
    pub sqs_parameters: Option<SqsParameters>,
    /// Raw passthroughs for target-type-specific parameters that the
    /// current fakecloud firing pipeline does not interpret
    /// (EcsParameters, EventBridgeParameters, KinesisParameters,
    /// SageMakerPipelineParameters). Stored as-is so GetSchedule
    /// round-trips what the caller sent.
    pub ecs_parameters: Option<Value>,
    pub eventbridge_parameters: Option<Value>,
    pub kinesis_parameters: Option<Value>,
    pub sagemaker_pipeline_parameters: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeadLetterConfig {
    pub arn: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryPolicy {
    pub maximum_event_age_in_seconds: Option<i64>,
    pub maximum_retry_attempts: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SqsParameters {
    pub message_group_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduleGroup {
    pub arn: String,
    pub name: String,
    pub state: String, // ACTIVE | DELETING
    pub creation_date: DateTime<Utc>,
    pub last_modification_date: DateTime<Utc>,
    pub tags: BTreeMap<String, String>,
}

/// Composite key: (group_name, schedule_name). Schedules are unique
/// within a group; the same schedule name can exist across groups.
pub type ScheduleKey = (String, String);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchedulerState {
    pub account_id: String,
    pub region: String,
    #[serde(default)]
    pub groups: BTreeMap<String, ScheduleGroup>,
    // JSON can't represent a tuple-keyed map, so we (de)serialize
    // schedules as a flat Vec<Schedule> and rebuild the in-memory
    // `(group, name)` index on read. Pre-refactor versions silently
    // dropped the entire map during save — the regression that broke
    // schedule-survives-restart.
    #[serde(default, with = "schedules_vec_serde")]
    pub schedules: BTreeMap<ScheduleKey, Schedule>,
}

mod schedules_vec_serde {
    use super::{Schedule, ScheduleKey};
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::collections::BTreeMap;

    pub fn serialize<S: Serializer>(
        schedules: &BTreeMap<ScheduleKey, Schedule>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        let mut sorted: Vec<&Schedule> = schedules.values().collect();
        sorted.sort_by(|a, b| {
            a.group_name
                .cmp(&b.group_name)
                .then_with(|| a.name.cmp(&b.name))
        });
        sorted.serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<BTreeMap<ScheduleKey, Schedule>, D::Error> {
        let v: Vec<Schedule> = Vec::deserialize(deserializer)?;
        Ok(v.into_iter()
            .map(|s| ((s.group_name.clone(), s.name.clone()), s))
            .collect())
    }
}

impl SchedulerState {
    pub fn new(account_id: &str, region: &str) -> Self {
        let now = Utc::now();
        let mut groups = BTreeMap::new();
        groups.insert(
            DEFAULT_GROUP.to_string(),
            ScheduleGroup {
                arn: group_arn(region, account_id, DEFAULT_GROUP),
                name: DEFAULT_GROUP.to_string(),
                state: "ACTIVE".to_string(),
                creation_date: now,
                last_modification_date: now,
                tags: BTreeMap::new(),
            },
        );
        Self {
            account_id: account_id.to_string(),
            region: region.to_string(),
            groups,
            schedules: BTreeMap::new(),
        }
    }

    pub fn reset(&mut self) {
        let now = Utc::now();
        self.groups.clear();
        self.schedules.clear();
        self.groups.insert(
            DEFAULT_GROUP.to_string(),
            ScheduleGroup {
                arn: group_arn(&self.region, &self.account_id, DEFAULT_GROUP),
                name: DEFAULT_GROUP.to_string(),
                state: "ACTIVE".to_string(),
                creation_date: now,
                last_modification_date: now,
                tags: BTreeMap::new(),
            },
        );
    }
}

impl AccountState for SchedulerState {
    fn new_for_account(account_id: &str, region: &str, _endpoint: &str) -> Self {
        Self::new(account_id, region)
    }
}

pub type SharedSchedulerState = Arc<RwLock<MultiAccountState<SchedulerState>>>;

/// Bumped whenever the on-disk shape of `SchedulerSnapshot` changes.
/// Schema version 1 is the initial format introduced by Batch 3.
pub const SCHEDULER_SNAPSHOT_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
pub struct SchedulerSnapshot {
    pub schema_version: u32,
    pub accounts: MultiAccountState<SchedulerState>,
}

/// Build an EventBridge Scheduler schedule ARN.
/// Format: `arn:<partition>:scheduler:<region>:<account>:schedule/<group>/<name>`,
/// in `region`'s partition.
pub fn schedule_arn(region: &str, account_id: &str, group: &str, name: &str) -> String {
    Arn::regional(
        "scheduler",
        region,
        account_id,
        &format!("schedule/{group}/{name}"),
    )
    .to_string()
}

/// Build a schedule-group ARN.
/// Format: `arn:<partition>:scheduler:<region>:<account>:schedule-group/<group>`,
/// in `region`'s partition.
pub fn group_arn(region: &str, account_id: &str, group: &str) -> String {
    Arn::regional(
        "scheduler",
        region,
        account_id,
        &format!("schedule-group/{group}"),
    )
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_seeds_default_group() {
        let s = SchedulerState::new("111122223333", "us-east-1");
        assert!(s.groups.contains_key(DEFAULT_GROUP));
        let g = &s.groups[DEFAULT_GROUP];
        assert_eq!(g.state, "ACTIVE");
        assert_eq!(
            g.arn,
            "arn:aws:scheduler:us-east-1:111122223333:schedule-group/default"
        );
    }

    #[test]
    fn reset_keeps_default_group_and_clears_schedules() {
        let mut s = SchedulerState::new("111122223333", "us-east-1");
        s.schedules.insert(
            ("default".to_string(), "s1".to_string()),
            Schedule {
                arn: "arn".to_string(),
                name: "s1".to_string(),
                group_name: "default".to_string(),
                schedule_expression: "rate(1 minute)".to_string(),
                schedule_expression_timezone: None,
                start_date: None,
                end_date: None,
                description: None,
                state: "ENABLED".to_string(),
                kms_key_arn: None,
                action_after_completion: "NONE".to_string(),
                flexible_time_window: FlexibleTimeWindow::default(),
                target: Target {
                    arn: "arn:aws:sqs:us-east-1:111122223333:q".to_string(),
                    role_arn: "arn:aws:iam::111122223333:role/r".to_string(),
                    input: None,
                    dead_letter_config: None,
                    retry_policy: None,
                    sqs_parameters: None,
                    ecs_parameters: None,
                    eventbridge_parameters: None,
                    kinesis_parameters: None,
                    sagemaker_pipeline_parameters: None,
                },
                creation_date: Utc::now(),
                last_modification_date: Utc::now(),
                last_fired: None,
            },
        );
        s.groups.insert(
            "custom".to_string(),
            ScheduleGroup {
                arn: "arn".to_string(),
                name: "custom".to_string(),
                state: "ACTIVE".to_string(),
                creation_date: Utc::now(),
                last_modification_date: Utc::now(),
                tags: BTreeMap::new(),
            },
        );
        s.reset();
        assert!(s.schedules.is_empty());
        assert_eq!(s.groups.len(), 1);
        assert!(s.groups.contains_key(DEFAULT_GROUP));
    }

    #[test]
    fn arn_builders() {
        assert_eq!(
            schedule_arn("us-east-1", "1", "g", "n"),
            "arn:aws:scheduler:us-east-1:1:schedule/g/n"
        );
        assert_eq!(
            group_arn("us-east-1", "1", "g"),
            "arn:aws:scheduler:us-east-1:1:schedule-group/g"
        );
    }
}
