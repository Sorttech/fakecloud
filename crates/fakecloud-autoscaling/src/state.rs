//! In-memory state for EC2 Auto Scaling.

use std::collections::BTreeMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};

pub type SharedAutoScalingState = Arc<RwLock<AutoScalingAccounts>>;

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct AutoScalingAccounts {
    pub accounts: BTreeMap<String, AccountState>,
}

impl AutoScalingAccounts {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get_or_create(&mut self, account_id: &str) -> &mut AccountState {
        self.accounts.entry(account_id.to_string()).or_default()
    }
}

/// Versioned on-disk persistence snapshot for EC2 Auto Scaling.
#[derive(Debug, Serialize, Deserialize)]
pub struct AutoScalingSnapshot {
    pub schema_version: u32,
    #[serde(default)]
    pub accounts: Option<AutoScalingAccounts>,
}

pub const AUTOSCALING_SNAPSHOT_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct AccountState {
    /// Launch configurations keyed by name.
    pub launch_configurations: BTreeMap<String, LaunchConfiguration>,
    /// Auto Scaling groups keyed by name.
    pub groups: BTreeMap<String, AutoScalingGroup>,
    /// Scaling activities, newest first.
    pub activities: Vec<ScalingActivity>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LaunchConfiguration {
    pub name: String,
    pub arn: String,
    pub image_id: String,
    pub instance_type: String,
    #[serde(default)]
    pub key_name: Option<String>,
    #[serde(default)]
    pub security_groups: Vec<String>,
    #[serde(default)]
    pub user_data: Option<String>,
    #[serde(default)]
    pub iam_instance_profile: Option<String>,
    #[serde(default)]
    pub associate_public_ip_address: Option<bool>,
    /// `InstanceMonitoring.Enabled` — AWS/Terraform default is `true`.
    #[serde(default = "default_true")]
    pub instance_monitoring: bool,
    #[serde(default)]
    pub ebs_optimized: bool,
    #[serde(default)]
    pub spot_price: Option<String>,
    #[serde(default)]
    pub placement_tenancy: Option<String>,
    /// `BlockDeviceMappings`: the EBS volumes every instance launched from
    /// this configuration gets.
    #[serde(default)]
    pub block_device_mappings: Vec<BlockDeviceMapping>,
    /// `MetadataOptions` (instance metadata service settings).
    #[serde(default)]
    pub metadata_options: Option<InstanceMetadataOptions>,
    pub created_time: DateTime<Utc>,
}

/// A launch configuration block-device mapping.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockDeviceMapping {
    pub device_name: String,
    #[serde(default)]
    pub virtual_name: Option<String>,
    #[serde(default)]
    pub no_device: bool,
    #[serde(default)]
    pub ebs: Option<Ebs>,
}

/// The EBS volume of a launch configuration block-device mapping.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ebs {
    #[serde(default)]
    pub snapshot_id: Option<String>,
    #[serde(default)]
    pub volume_size: Option<i64>,
    #[serde(default)]
    pub volume_type: Option<String>,
    #[serde(default)]
    pub delete_on_termination: Option<bool>,
    #[serde(default)]
    pub iops: Option<i64>,
    #[serde(default)]
    pub encrypted: Option<bool>,
    #[serde(default)]
    pub throughput: Option<i64>,
}

/// A launch configuration's `MetadataOptions`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstanceMetadataOptions {
    #[serde(default)]
    pub http_tokens: Option<String>,
    #[serde(default)]
    pub http_put_response_hop_limit: Option<i64>,
    #[serde(default)]
    pub http_endpoint: Option<String>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutoScalingGroup {
    pub name: String,
    pub arn: String,
    #[serde(default)]
    pub launch_configuration_name: Option<String>,
    /// (LaunchTemplateId, LaunchTemplateName, Version) when launched from a
    /// launch template instead of a launch configuration.
    #[serde(default)]
    pub launch_template: Option<LaunchTemplateSpec>,
    pub min_size: i64,
    pub max_size: i64,
    pub desired_capacity: i64,
    pub default_cooldown: i64,
    #[serde(default)]
    pub availability_zones: Vec<String>,
    /// `VPCZoneIdentifier` — comma-separated subnet ids.
    #[serde(default)]
    pub vpc_zone_identifier: Option<String>,
    pub health_check_type: String,
    pub health_check_grace_period: i64,
    #[serde(default)]
    pub target_group_arns: Vec<String>,
    #[serde(default)]
    pub load_balancer_names: Vec<String>,
    #[serde(default)]
    pub new_instances_protected_from_scale_in: bool,
    pub created_time: DateTime<Utc>,
    #[serde(default)]
    pub instances: Vec<AsgInstance>,
    /// ASG tags (propagate-at-launch tracked per tag).
    #[serde(default)]
    pub tags: Vec<AsgTag>,
    /// Set during a DeleteAutoScalingGroup that is draining instances.
    #[serde(default)]
    pub status: Option<String>,
    /// `ServiceLinkedRoleARN` — defaults to the AWSServiceRoleForAutoScaling SLR.
    #[serde(default)]
    pub service_linked_role_arn: String,
    /// `MixedInstancesPolicy`: a launch template plus instance-type /
    /// template overrides, launched instead of `launch_template`.
    #[serde(default)]
    pub mixed_instances_policy: Option<MixedInstancesPolicy>,
}

/// An Auto Scaling group's `MixedInstancesPolicy`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MixedInstancesPolicy {
    /// `LaunchTemplate.LaunchTemplateSpecification`.
    pub launch_template: LaunchTemplateSpec,
    /// `LaunchTemplate.Overrides`, in priority order.
    #[serde(default)]
    pub overrides: Vec<LaunchTemplateOverride>,
    #[serde(default)]
    pub instances_distribution: Option<InstancesDistribution>,
}

/// One `LaunchTemplateOverrides` entry.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LaunchTemplateOverride {
    #[serde(default)]
    pub instance_type: Option<String>,
    #[serde(default)]
    pub weighted_capacity: Option<String>,
    /// A different launch template for this instance type.
    #[serde(default)]
    pub launch_template_specification: Option<LaunchTemplateSpec>,
}

/// A `MixedInstancesPolicy`'s `InstancesDistribution`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InstancesDistribution {
    #[serde(default)]
    pub on_demand_allocation_strategy: Option<String>,
    #[serde(default)]
    pub on_demand_base_capacity: Option<i64>,
    #[serde(default)]
    pub on_demand_percentage_above_base_capacity: Option<i64>,
    #[serde(default)]
    pub spot_allocation_strategy: Option<String>,
    #[serde(default)]
    pub spot_instance_pools: Option<i64>,
    #[serde(default)]
    pub spot_max_price: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchTemplateSpec {
    #[serde(default)]
    pub launch_template_id: Option<String>,
    #[serde(default)]
    pub launch_template_name: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AsgInstance {
    pub instance_id: String,
    pub availability_zone: String,
    /// `Pending` | `InService` | `Terminating` | `Terminated`.
    pub lifecycle_state: String,
    /// `Healthy` | `Unhealthy`.
    pub health_status: String,
    #[serde(default)]
    pub launch_configuration_name: Option<String>,
    #[serde(default)]
    pub protected_from_scale_in: bool,
    /// The instance type it was launched with.
    #[serde(default)]
    pub instance_type: Option<String>,
    /// The launch template (id, name, resolved version) it was launched from.
    #[serde(default)]
    pub launch_template: Option<LaunchTemplateSpec>,
    /// Its `WeightedCapacity` (mixed-instances override weight), counted
    /// toward the group's desired capacity; `None` counts as 1.
    #[serde(default)]
    pub weighted_capacity: Option<String>,
    /// `spot` for a Spot instance (a mixed-instances policy's Spot share or a
    /// launch configuration's `SpotPrice`), `None` for on-demand.
    #[serde(default)]
    pub lifecycle: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AsgTag {
    pub key: String,
    pub value: String,
    #[serde(default)]
    pub propagate_at_launch: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScalingActivity {
    pub activity_id: String,
    pub auto_scaling_group_name: String,
    pub description: String,
    pub cause: String,
    pub start_time: DateTime<Utc>,
    #[serde(default)]
    pub end_time: Option<DateTime<Utc>>,
    /// `Successful` | `InProgress` | `Failed`.
    pub status_code: String,
    pub progress: i64,
    #[serde(default)]
    pub details: String,
    /// Why a `Failed` activity failed.
    #[serde(default)]
    pub status_message: Option<String>,
}
