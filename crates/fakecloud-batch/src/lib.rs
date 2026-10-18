//! AWS Batch (`batch`) restJson1 service for fakecloud.
//!
//! Full control plane (compute environments, job queues, job definitions,
//! scheduling policies, consumable resources, service environments, quota
//! shares, tags) plus job execution: `SubmitJob` runs a real container-backed
//! ECS task when a container runtime is attached. Service jobs (SageMaker
//! Training through Batch) are queued and validated but wait at `RUNNABLE`,
//! since fakecloud has no SageMaker training executor to dispatch them to.

mod extended;
pub mod service;
pub mod state;

pub use service::{batch_arn, normalize_compute_environment_order, BatchService};
pub use state::{
    BatchAccounts, BatchSnapshot, BatchState, SharedBatchState, BATCH_SNAPSHOT_SCHEMA_VERSION,
};
