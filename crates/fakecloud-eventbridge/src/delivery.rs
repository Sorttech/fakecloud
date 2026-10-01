use std::sync::Arc;

use chrono::Utc;

use fakecloud_core::delivery::{DeliveryBus, EventBridgeDelivery};
use fakecloud_lambda::runtime::ContainerRuntime;
use fakecloud_lambda::SharedLambdaState;
use fakecloud_logs::SharedLogsState;

use crate::service::{dispatch_event_target, matches_pattern, EventDispatchContext};
use crate::state::{PutEvent, SharedEventBridgeState};

/// The non-bus plumbing a rule target dispatch needs beyond the
/// [`DeliveryBus`]: Lambda state + container runtime (Lambda targets are
/// recorded in Lambda's invocation log and executed directly, not via the
/// bus), and CloudWatch Logs state + its persist hook (Logs targets write the
/// log group directly).
///
/// PutEvents (`EventBridgeService`), the rule `Scheduler` and every
/// cross-service [`EventBridgeDeliveryImpl`] take the same value, so an event
/// another service publishes (S3, SES, ECS, RDS, Step Functions, ...) reaches
/// the same target types as one sent with PutEvents. A field left `None`
/// means that target type is only recorded, never delivered, and dispatch
/// logs a warning when it hits one.
#[derive(Clone, Default)]
pub struct EventTargetWiring {
    pub lambda_state: Option<SharedLambdaState>,
    pub logs_state: Option<SharedLogsState>,
    pub logs_persist: Option<fakecloud_persistence::SnapshotHook>,
    pub container_runtime: Option<Arc<ContainerRuntime>>,
}

/// Implements EventBridgeDelivery so other services (S3, SES, ECS, ...) can
/// put events on an EventBridge bus with full rule matching and target
/// delivery.
pub struct EventBridgeDeliveryImpl {
    state: SharedEventBridgeState,
    delivery: Arc<DeliveryBus>,
    wiring: EventTargetWiring,
}

impl EventBridgeDeliveryImpl {
    pub fn new(state: SharedEventBridgeState, delivery: Arc<DeliveryBus>) -> Self {
        Self {
            state,
            delivery,
            wiring: EventTargetWiring::default(),
        }
    }

    /// Wire every non-bus target dependency at once (see [`EventTargetWiring`]).
    pub fn with_target_wiring(mut self, wiring: EventTargetWiring) -> Self {
        self.wiring = wiring;
        self
    }

    pub fn with_lambda(mut self, lambda_state: SharedLambdaState) -> Self {
        self.wiring.lambda_state = Some(lambda_state);
        self
    }

    pub fn with_logs(mut self, logs_state: SharedLogsState) -> Self {
        self.wiring.logs_state = Some(logs_state);
        self
    }

    /// Wire the CloudWatch Logs persist hook so events this bus delivers to a
    /// Logs target are written through to the Logs snapshot (see
    /// `EventDispatchContext::logs_persist`).
    pub fn with_logs_persist(mut self, hook: fakecloud_persistence::SnapshotHook) -> Self {
        self.wiring.logs_persist = Some(hook);
        self
    }

    pub fn with_runtime(mut self, runtime: Arc<ContainerRuntime>) -> Self {
        self.wiring.container_runtime = Some(runtime);
        self
    }
}

/// An [`EventBridgeDelivery`] whose real implementation is supplied after
/// construction. Breaks the construction cycle where the bus EventBridge
/// targets deliver through (which starts Step Functions executions) must
/// itself hold an EventBridge sender (for the interpreter's
/// `events:putEvents` task). Events put before [`Self::set`] is called are
/// dropped with a warning.
#[derive(Clone, Default)]
pub struct DeferredEventBridgeDelivery {
    inner: Arc<std::sync::OnceLock<Arc<dyn EventBridgeDelivery>>>,
}

impl DeferredEventBridgeDelivery {
    pub fn new() -> Self {
        Self::default()
    }

    /// Install the real sender. Only the first call takes effect.
    pub fn set(&self, delivery: Arc<dyn EventBridgeDelivery>) {
        if self.inner.set(delivery).is_err() {
            tracing::warn!("deferred EventBridge delivery already set; ignoring");
        }
    }

    fn get(&self, source: &str, detail_type: &str) -> Option<&Arc<dyn EventBridgeDelivery>> {
        let inner = self.inner.get();
        if inner.is_none() {
            tracing::warn!(
                source,
                detail_type,
                "EventBridge delivery not wired yet; dropping event"
            );
        }
        inner
    }
}

impl EventBridgeDelivery for DeferredEventBridgeDelivery {
    fn put_event(&self, source: &str, detail_type: &str, detail: &str, event_bus_name: &str) {
        if let Some(inner) = self.get(source, detail_type) {
            inner.put_event(source, detail_type, detail, event_bus_name);
        }
    }

    fn put_event_to_account(
        &self,
        source: &str,
        detail_type: &str,
        detail: &str,
        event_bus_name: &str,
        target_account_id: &str,
    ) {
        if let Some(inner) = self.get(source, detail_type) {
            inner.put_event_to_account(
                source,
                detail_type,
                detail,
                event_bus_name,
                target_account_id,
            );
        }
    }
}

impl EventBridgeDeliveryImpl {
    fn put_event_in_account(
        &self,
        source: &str,
        detail_type: &str,
        detail: &str,
        event_bus_name: &str,
        target_account_id: Option<&str>,
    ) {
        let event_id = uuid::Uuid::new_v4().to_string();
        let now = Utc::now();

        let event = PutEvent {
            event_id: event_id.clone(),
            source: source.to_string(),
            detail_type: detail_type.to_string(),
            detail: detail.to_string(),
            event_bus_name: event_bus_name.to_string(),
            time: now,
            resources: Vec::new(),
        };

        let mut accounts = self.state.write();
        let state = match target_account_id {
            Some(account_id) if !account_id.is_empty() => accounts.get_or_create(account_id),
            _ => accounts.default_mut(),
        };
        state.events.push(event);

        // Find matching rules and their targets
        let account_id = state.account_id.clone();
        let region = state.region.clone();
        let matching_targets: Vec<(String, crate::state::EventTarget)> = state
            .rules
            .values()
            .filter(|r| {
                r.event_bus_name == event_bus_name
                    && r.state == "ENABLED"
                    && matches_pattern(
                        r.event_pattern.as_deref(),
                        source,
                        detail_type,
                        detail,
                        &account_id,
                        &region,
                        &[],
                        &event_id,
                        &now.to_rfc3339(),
                    )
            })
            .flat_map(|r| r.targets.iter().map(|t| (r.arn.clone(), t.clone())))
            .collect();

        // Drop the lock before delivering
        drop(accounts);

        if matching_targets.is_empty() {
            return;
        }

        // Build the EventBridge event envelope
        let detail_value: serde_json::Value =
            serde_json::from_str(detail).unwrap_or(serde_json::json!({}));
        let event_json = serde_json::json!({
            "version": "0",
            "id": event_id,
            "source": source,
            "account": account_id,
            "detail-type": detail_type,
            "detail": detail_value,
            "time": now.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
            "region": region,
            "resources": [],
        });
        let resolved_account = if let Some(acct) = target_account_id {
            acct.to_string()
        } else {
            account_id.clone()
        };
        let ctx = EventDispatchContext {
            state: &self.state,
            delivery: &self.delivery,
            lambda_state: self.wiring.lambda_state.as_ref(),
            logs_state: self.wiring.logs_state.as_ref(),
            logs_persist: self.wiring.logs_persist.as_ref(),
            container_runtime: &self.wiring.container_runtime,
            account_id: &resolved_account,
            region: &region,
        };
        for (rule_arn, target) in matching_targets {
            dispatch_event_target(
                &ctx,
                &target,
                &event_json,
                &event_id,
                detail_type,
                Some(&rule_arn),
            );
        }
    }
}

impl EventBridgeDelivery for EventBridgeDeliveryImpl {
    fn put_event(&self, source: &str, detail_type: &str, detail: &str, event_bus_name: &str) {
        self.put_event_in_account(source, detail_type, detail, event_bus_name, None);
    }

    fn put_event_to_account(
        &self,
        source: &str,
        detail_type: &str,
        detail: &str,
        event_bus_name: &str,
        target_account_id: &str,
    ) {
        self.put_event_in_account(
            source,
            detail_type,
            detail,
            event_bus_name,
            Some(target_account_id),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{EventRule, EventTarget as EbTarget, SharedEventBridgeState};
    use fakecloud_aws::arn::Arn;
    use fakecloud_core::delivery::{SnsDelivery, SqsDelivery};
    use parking_lot::RwLock;
    use std::collections::{BTreeMap, HashMap};
    use std::sync::Mutex;

    #[derive(Default)]
    struct Recorder {
        sqs: Mutex<Vec<(String, String)>>,
        sns: Mutex<Vec<(String, String, Option<String>)>>,
    }

    impl SqsDelivery for Recorder {
        fn deliver_to_queue(&self, arn: &str, body: &str, _: &HashMap<String, String>) {
            self.sqs
                .lock()
                .unwrap()
                .push((arn.to_string(), body.to_string()));
        }
        fn deliver_to_queue_with_attrs(
            &self,
            arn: &str,
            body: &str,
            _: &HashMap<String, fakecloud_core::delivery::SqsMessageAttribute>,
            _: Option<&str>,
            _: Option<&str>,
        ) {
            self.sqs
                .lock()
                .unwrap()
                .push((arn.to_string(), body.to_string()));
        }
    }

    impl SnsDelivery for Recorder {
        fn publish_to_topic(&self, arn: &str, msg: &str, subject: Option<&str>) {
            self.sns.lock().unwrap().push((
                arn.to_string(),
                msg.to_string(),
                subject.map(|s| s.to_string()),
            ));
        }
    }

    fn make_shared() -> SharedEventBridgeState {
        Arc::new(RwLock::new(
            fakecloud_core::multi_account::MultiAccountState::new("123456789012", "us-east-1", ""),
        ))
    }

    fn make_rule(name: &str, pattern: Option<&str>, target_arn: &str) -> EventRule {
        EventRule {
            name: name.to_string(),
            arn: Arn::new(
                "events",
                "us-east-1",
                "123456789012",
                &format!("rule/{name}"),
            )
            .to_string(),
            event_bus_name: "default".to_string(),
            event_pattern: pattern.map(|s| s.to_string()),
            schedule_expression: None,
            state: "ENABLED".to_string(),
            description: None,
            role_arn: None,
            managed_by: None,
            created_by: None,
            targets: vec![EbTarget {
                id: "t1".to_string(),
                arn: target_arn.to_string(),
                input: None,
                input_path: None,
                input_transformer: None,
                sqs_parameters: None,
                ..Default::default()
            }],
            tags: BTreeMap::new(),
            last_fired: None,
        }
    }

    #[test]
    fn put_event_appends_to_events_log() {
        let state = make_shared();
        let bus = Arc::new(DeliveryBus::new());
        let delivery = EventBridgeDeliveryImpl::new(state.clone(), bus);
        delivery.put_event("my.source", "MyType", r#"{"k":"v"}"#, "default");
        let guard = state.read();
        let default = guard.default_ref();
        assert_eq!(default.events.len(), 1);
        assert_eq!(default.events[0].source, "my.source");
        assert_eq!(default.events[0].detail_type, "MyType");
    }

    #[test]
    fn put_event_dispatches_matching_sqs_target() {
        let state = make_shared();
        let q_arn = "arn:aws:sqs:us-east-1:123456789012:q".to_string();
        {
            let mut s_accounts = state.write();
            let s = s_accounts.default_mut();
            let rule = make_rule("r", None, &q_arn);
            s.rules
                .insert(("default".to_string(), "r".to_string()), rule);
        }
        let recorder = Arc::new(Recorder::default());
        let bus = Arc::new(DeliveryBus::new().with_sqs(recorder.clone()));
        let delivery = EventBridgeDeliveryImpl::new(state, bus);
        delivery.put_event("app", "Changed", r#"{"x":1}"#, "default");
        let calls = recorder.sqs.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, q_arn);
        let env: serde_json::Value = serde_json::from_str(&calls[0].1).unwrap();
        assert_eq!(env["detail-type"], "Changed");
        assert_eq!(env["source"], "app");
    }

    #[test]
    fn put_event_dispatches_to_sns_target() {
        let state = make_shared();
        let topic_arn = "arn:aws:sns:us-east-1:123456789012:t".to_string();
        {
            let mut s_accounts = state.write();
            let s = s_accounts.default_mut();
            let rule = make_rule("r", None, &topic_arn);
            s.rules
                .insert(("default".to_string(), "r".to_string()), rule);
        }
        let recorder = Arc::new(Recorder::default());
        let bus = Arc::new(DeliveryBus::new().with_sns(recorder.clone()));
        let delivery = EventBridgeDeliveryImpl::new(state, bus);
        delivery.put_event("app", "Changed", r#"{}"#, "default");
        let calls = recorder.sns.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, topic_arn);
        assert_eq!(calls[0].2.as_deref(), Some("Changed"));
    }

    #[test]
    fn put_event_skips_disabled_rule() {
        let state = make_shared();
        let q_arn = "arn:aws:sqs:us-east-1:123456789012:q".to_string();
        {
            let mut s_accounts = state.write();
            let s = s_accounts.default_mut();
            let mut rule = make_rule("r", None, &q_arn);
            rule.state = "DISABLED".to_string();
            s.rules
                .insert(("default".to_string(), "r".to_string()), rule);
        }
        let recorder = Arc::new(Recorder::default());
        let bus = Arc::new(DeliveryBus::new().with_sqs(recorder.clone()));
        let delivery = EventBridgeDeliveryImpl::new(state, bus);
        delivery.put_event("app", "Changed", r#"{}"#, "default");
        assert!(recorder.sqs.lock().unwrap().is_empty());
    }

    #[test]
    fn put_event_skips_other_bus_rule() {
        let state = make_shared();
        let q_arn = "arn:aws:sqs:us-east-1:123456789012:q".to_string();
        {
            let mut s_accounts = state.write();
            let s = s_accounts.default_mut();
            let mut rule = make_rule("r", None, &q_arn);
            rule.event_bus_name = "custom-bus".to_string();
            s.rules
                .insert(("custom-bus".to_string(), "r".to_string()), rule);
        }
        let recorder = Arc::new(Recorder::default());
        let bus = Arc::new(DeliveryBus::new().with_sqs(recorder.clone()));
        let delivery = EventBridgeDeliveryImpl::new(state, bus);
        delivery.put_event("app", "Changed", r#"{}"#, "default");
        assert!(recorder.sqs.lock().unwrap().is_empty());
    }

    #[test]
    fn put_event_handles_invalid_detail_json_gracefully() {
        let state = make_shared();
        let q_arn = "arn:aws:sqs:us-east-1:123456789012:q".to_string();
        {
            let mut s_accounts = state.write();
            let s = s_accounts.default_mut();
            let rule = make_rule("r", None, &q_arn);
            s.rules
                .insert(("default".to_string(), "r".to_string()), rule);
        }
        let recorder = Arc::new(Recorder::default());
        let bus = Arc::new(DeliveryBus::new().with_sqs(recorder.clone()));
        let delivery = EventBridgeDeliveryImpl::new(state, bus);
        delivery.put_event("app", "Type", "not-json", "default");
        let calls = recorder.sqs.lock().unwrap();
        assert_eq!(calls.len(), 1);
        let env: serde_json::Value = serde_json::from_str(&calls[0].1).unwrap();
        assert_eq!(env["detail"], serde_json::json!({}));
    }

    #[test]
    fn put_event_to_account_writes_to_target_account_bus() {
        let state = make_shared();
        let bus = Arc::new(DeliveryBus::new());
        let delivery = EventBridgeDeliveryImpl::new(state.clone(), bus);
        delivery.put_event_to_account("scheduler", "Fired", r#"{}"#, "default", "999988887777");

        let guard = state.read();
        let target = guard
            .get("999988887777")
            .expect("target account should be created on demand");
        assert_eq!(target.events.len(), 1);
        assert_eq!(target.events[0].source, "scheduler");
        // The default account's bus should be untouched.
        assert!(guard.default_ref().events.is_empty());
    }

    #[test]
    fn put_event_to_account_dispatches_to_rules_in_target_account() {
        let state = make_shared();
        let q_arn = "arn:aws:sqs:us-east-1:999988887777:cross-q".to_string();
        {
            let mut s_accounts = state.write();
            let s = s_accounts.get_or_create("999988887777");
            let rule = make_rule("xacct-rule", None, &q_arn);
            s.rules
                .insert(("default".to_string(), "xacct-rule".to_string()), rule);
        }
        let recorder = Arc::new(Recorder::default());
        let bus = Arc::new(DeliveryBus::new().with_sqs(recorder.clone()));
        let delivery = EventBridgeDeliveryImpl::new(state, bus);
        delivery.put_event_to_account(
            "scheduler",
            "Cross",
            r#"{"hi":1}"#,
            "default",
            "999988887777",
        );
        let calls = recorder.sqs.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, q_arn);
    }

    fn insert_rule(state: &SharedEventBridgeState, rule: EventRule) {
        let mut accounts = state.write();
        accounts
            .default_mut()
            .rules
            .insert(("default".to_string(), rule.name.clone()), rule);
    }

    /// Regression for #2628: an event another service (S3) publishes through
    /// the cross-service delivery must reach a Lambda target exactly like
    /// PutEvents -- recorded in Lambda's invocation log (what
    /// `/_fakecloud/lambda/invocations` serves) rather than dropped because
    /// the delivery impl had no Lambda wiring.
    #[test]
    fn put_event_with_target_wiring_records_lambda_invocation() {
        let state = make_shared();
        let fn_arn = "arn:aws:lambda:us-east-1:123456789012:function:my-fn";
        insert_rule(
            &state,
            make_rule(
                "s3-to-fn",
                Some(r#"{"source":["aws.s3"],"detail-type":["Object Created"]}"#),
                fn_arn,
            ),
        );
        let lambda_state: SharedLambdaState = Arc::new(RwLock::new(
            fakecloud_core::multi_account::MultiAccountState::new("123456789012", "us-east-1", ""),
        ));
        let delivery = EventBridgeDeliveryImpl::new(state.clone(), Arc::new(DeliveryBus::new()))
            .with_target_wiring(EventTargetWiring {
                lambda_state: Some(lambda_state.clone()),
                ..Default::default()
            });

        delivery.put_event(
            "aws.s3",
            "Object Created",
            r#"{"bucket":{"name":"eb-bucket"},"object":{"key":"anything"}}"#,
            "default",
        );

        let accounts = lambda_state.read();
        let invocations = &accounts.default_ref().invocations;
        assert_eq!(invocations.len(), 1);
        assert_eq!(invocations[0].function_arn, fn_arn);
        assert_eq!(invocations[0].source, "aws:events");
        let payload: serde_json::Value = serde_json::from_str(&invocations[0].payload).unwrap();
        assert_eq!(payload["source"], "aws.s3");
        assert_eq!(payload["detail-type"], "Object Created");
        assert_eq!(payload["detail"]["bucket"]["name"], "eb-bucket");
        // EventBridge's own delivery record is kept too.
        assert_eq!(state.read().default_ref().lambda_invocations.len(), 1);
    }

    /// A cross-service event matched by a rule with a CloudWatch Logs target
    /// lands in the log group, as with PutEvents.
    #[test]
    fn put_event_with_target_wiring_writes_logs_target() {
        let state = make_shared();
        let group_arn = "arn:aws:logs:us-east-1:123456789012:log-group:/aws/events/s3";
        insert_rule(
            &state,
            make_rule("s3-to-logs", Some(r#"{"source":["aws.s3"]}"#), group_arn),
        );
        let logs_state: SharedLogsState = Arc::new(RwLock::new(
            fakecloud_core::multi_account::MultiAccountState::new("123456789012", "us-east-1", ""),
        ));
        let delivery = EventBridgeDeliveryImpl::new(state, Arc::new(DeliveryBus::new()))
            .with_target_wiring(EventTargetWiring {
                logs_state: Some(logs_state.clone()),
                ..Default::default()
            });

        delivery.put_event("aws.s3", "Object Created", r#"{"k":1}"#, "default");

        let accounts = logs_state.read();
        let group = accounts
            .default_ref()
            .log_groups
            .get("/aws/events/s3")
            .expect("log group auto-created by the Logs target");
        let events = &group.log_streams["events"].events;
        assert_eq!(events.len(), 1);
        let payload: serde_json::Value = serde_json::from_str(&events[0].message).unwrap();
        assert_eq!(payload["source"], "aws.s3");
    }

    #[test]
    fn deferred_delivery_forwards_once_set() {
        let state = make_shared();
        let deferred = DeferredEventBridgeDelivery::new();
        // Before the real sender is bound the event is dropped, not panicked on.
        deferred.put_event("early", "T", "{}", "default");
        assert!(state.read().default_ref().events.is_empty());

        deferred.set(Arc::new(EventBridgeDeliveryImpl::new(
            state.clone(),
            Arc::new(DeliveryBus::new()),
        )));
        deferred.put_event("app", "T", "{}", "default");
        deferred.put_event_to_account("app", "T", "{}", "default", "999988887777");

        let accounts = state.read();
        assert_eq!(accounts.default_ref().events.len(), 1);
        assert_eq!(accounts.default_ref().events[0].source, "app");
        assert_eq!(accounts.get("999988887777").unwrap().events.len(), 1);
    }
}
