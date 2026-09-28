//! EventBridge implementation of [`ResourcePolicyProvider`].
//!
//! EventBridge persists per-bus access policies as JSON on
//! [`crate::state::EventBus::policy`]. `PutPermission`, `RemovePermission`,
//! and direct `policy` writes via `PutPermission` all settle into that
//! single slot. This file is the read-side bridge that dispatch consults
//! via `evaluate_with_resource_policy`, so cross-account `PutEvents`
//! callers go through Allow-union and explicit-Deny enforcement against
//! the bus's policy in addition to identity policies and SCPs.
//!
//! Mirrors `fakecloud_sns::resource_policy` intentionally: single-service
//! gate, ARN parsing, state lookup, return `None` for anything not owned
//! here so providers compose safely.

use std::sync::Arc;

use fakecloud_core::auth::ResourcePolicyProvider;

use crate::state::SharedEventBridgeState;

/// Concrete [`ResourcePolicyProvider`] backed by the in-memory
/// EventBridge state. Server bootstrap clone-shares it via
/// [`fakecloud_core::auth::MultiResourcePolicyProvider`].
pub struct EventBridgeResourcePolicyProvider {
    state: SharedEventBridgeState,
}

impl EventBridgeResourcePolicyProvider {
    pub fn new(state: SharedEventBridgeState) -> Self {
        Self { state }
    }

    pub fn shared(state: SharedEventBridgeState) -> Arc<dyn ResourcePolicyProvider> {
        Arc::new(Self::new(state))
    }
}

impl ResourcePolicyProvider for EventBridgeResourcePolicyProvider {
    fn resource_policy(&self, service: &str, resource_arn: &str) -> Option<String> {
        if !service.eq_ignore_ascii_case("events") {
            return None;
        }
        if !is_event_bus_arn(resource_arn) {
            return None;
        }
        let accts = self.state.read();
        let acct = resource_arn.split(':').nth(4).unwrap_or("");
        let state = accts.get(acct).unwrap_or_else(|| accts.default_ref());
        state
            .buses
            .values()
            .find(|b| b.answers_to(resource_arn))
            .and_then(|b| b.policy.as_ref())
            .map(|p| p.to_string())
    }
}

/// Light validity check on an EventBridge bus ARN. Accepts the
/// `arn:PARTITION:events:REGION:ACCOUNT:event-bus/NAME` shape and nothing else.
fn is_event_bus_arn(arn: &str) -> bool {
    let Some(rest) = fakecloud_aws::arn::arn_resource(arn, "events") else {
        return false;
    };
    let parts: Vec<&str> = rest.splitn(3, ':').collect();
    if parts.len() != 3 {
        return false;
    }
    if parts[0].is_empty() || parts[1].is_empty() {
        return false;
    }
    let resource = parts[2];
    if let Some(name) = resource.strip_prefix("event-bus/") {
        !name.is_empty()
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{EventBridgeState, EventBus};
    use chrono::Utc;
    use fakecloud_core::multi_account::MultiAccountState;
    use parking_lot::RwLock;
    use serde_json::json;
    use std::collections::BTreeMap;

    fn state_with_bus(arn: &str, policy: Option<serde_json::Value>) -> SharedEventBridgeState {
        let state = Arc::new(RwLock::new(MultiAccountState::<EventBridgeState>::new(
            "123456789012",
            "us-east-1",
            "http://localhost:4566",
        )));
        let name = arn
            .rsplit_once("event-bus/")
            .map(|(_, n)| n)
            .unwrap_or("default")
            .to_string();
        state.write().default_mut().buses.insert(
            name.clone(),
            EventBus {
                name: name.clone(),
                arn: arn.to_string(),
                description: None,
                policy,
                tags: BTreeMap::new(),
                creation_time: Utc::now(),
                last_modified_time: Utc::now(),
                kms_key_identifier: None,
                dead_letter_config: None,
            },
        );
        state
    }

    #[test]
    fn returns_stored_policy_for_event_bus_arn() {
        let policy = json!({"Version":"2012-10-17","Statement":[]});
        let arn = "arn:aws:events:us-east-1:123456789012:event-bus/my-bus";
        let state = state_with_bus(arn, Some(policy.clone()));
        let provider = EventBridgeResourcePolicyProvider::new(state);
        let raw = provider.resource_policy("events", arn).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed, policy);
    }

    #[test]
    fn returns_none_when_bus_has_no_policy() {
        let arn = "arn:aws:events:us-east-1:123456789012:event-bus/my-bus";
        let state = state_with_bus(arn, None);
        let provider = EventBridgeResourcePolicyProvider::new(state);
        assert_eq!(provider.resource_policy("events", arn), None);
    }

    #[test]
    fn returns_none_when_bus_missing() {
        let arn = "arn:aws:events:us-east-1:123456789012:event-bus/other";
        let state = state_with_bus(arn, Some(json!({})));
        let provider = EventBridgeResourcePolicyProvider::new(state);
        assert_eq!(
            provider.resource_policy(
                "events",
                "arn:aws:events:us-east-1:123456789012:event-bus/my-bus"
            ),
            None
        );
    }

    #[test]
    fn returns_none_for_non_events_service_prefix() {
        let arn = "arn:aws:events:us-east-1:123456789012:event-bus/b";
        let state = state_with_bus(arn, Some(json!({})));
        let provider = EventBridgeResourcePolicyProvider::new(state);
        assert_eq!(provider.resource_policy("sns", arn), None);
        assert_eq!(provider.resource_policy("sqs", arn), None);
    }

    #[test]
    fn service_prefix_match_is_case_insensitive() {
        let arn = "arn:aws:events:us-east-1:123456789012:event-bus/b";
        let state = state_with_bus(arn, Some(json!({})));
        let provider = EventBridgeResourcePolicyProvider::new(state);
        assert!(provider.resource_policy("EVENTS", arn).is_some());
    }

    #[test]
    fn returns_none_for_malformed_arn() {
        let arn = "arn:aws:events:us-east-1:123456789012:event-bus/b";
        let state = state_with_bus(arn, Some(json!({})));
        let provider = EventBridgeResourcePolicyProvider::new(state);
        assert_eq!(provider.resource_policy("events", ""), None);
        assert_eq!(provider.resource_policy("events", "not-an-arn"), None);
        assert_eq!(provider.resource_policy("events", "arn:aws:events:"), None);
        assert_eq!(
            provider.resource_policy(
                "events",
                "arn:aws:events:us-east-1:123456789012:rule/my-rule"
            ),
            None
        );
    }

    #[test]
    fn is_event_bus_arn_rejects_empty_segments() {
        assert!(!is_event_bus_arn("arn:aws:events:::event-bus/b"));
        assert!(!is_event_bus_arn("arn:aws:events:us-east-1::event-bus/b"));
        assert!(!is_event_bus_arn(
            "arn:aws:events:us-east-1:123456789012:event-bus/"
        ));
    }

    #[test]
    fn shared_constructor_wraps_in_arc() {
        let arn = "arn:aws:events:us-east-1:123456789012:event-bus/b";
        let state = state_with_bus(arn, Some(json!({"x": 1})));
        let arc = EventBridgeResourcePolicyProvider::shared(state);
        let raw = arc.resource_policy("events", arn).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed, json!({"x": 1}));
    }
}
