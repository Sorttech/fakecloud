//! Test support for crates that report KMS keys: a KMS hook over fresh state,
//! and an assertion that a reported ARN is the AWS-managed key for a service
//! in a given region. Compiled for this crate's tests and, through the
//! `test-util` feature, for other crates' dev-dependencies only.

use std::sync::Arc;

use fakecloud_core::delivery::KmsHook;

use crate::hook::KmsServiceHook;
use crate::state::SharedKmsState;

/// A KMS hook over fresh, empty KMS state, plus that state for assertions.
pub fn kms_hook(account_id: &str) -> (SharedKmsState, Arc<dyn KmsHook>) {
    let state: SharedKmsState = Arc::new(parking_lot::RwLock::new(
        fakecloud_core::multi_account::MultiAccountState::new(account_id, "us-east-1", ""),
    ));
    let hook = KmsServiceHook::new(state.clone(), Default::default());
    (state, Arc::new(hook))
}

/// `arn` is a real key in `state`: in `account_id` and `region` (and the
/// region's partition), AWS-managed, and the AWS-managed key `alias`
/// (`alias/aws/<service>`) stands for in that region: recorded as the
/// region's AWS-managed key and targeted by the region's alias.
pub fn assert_aws_managed_key(
    state: &SharedKmsState,
    account_id: &str,
    region: &str,
    arn: &str,
    alias: &str,
) {
    let expected_prefix = crate::state::kms_key_arn(region, account_id, "");
    assert!(
        arn.starts_with(&expected_prefix),
        "{arn} is not a key ARN in {region} (expected prefix {expected_prefix})"
    );
    let accounts = state.read();
    let s = accounts
        .get(account_id)
        .unwrap_or_else(|| panic!("no KMS state for account {account_id}"));
    let key = s
        .keys
        .values()
        .find(|k| k.arn == arn)
        .unwrap_or_else(|| panic!("{arn} is not a key in KMS"));
    assert_eq!(key.key_manager, "AWS", "{arn} is not AWS-managed");
    let slot = crate::state::aws_managed_key_slot(region, alias);
    let recorded = s.aws_managed_keys.get(&slot) == Some(&key.key_id);
    let aliased = s.alias_target(region, alias) == Some(key.key_id.as_str());
    assert!(
        recorded && aliased,
        "{arn} is not the {alias} key of {region} (recorded: {recorded}, aliased: {aliased})"
    );
}
