//! Custom-resource response signals.
//!
//! A CloudFormation custom resource does not report its outcome by returning a
//! value: the handler PUTs a JSON body to the pre-signed `ResponseURL` carried
//! on the event, and CloudFormation waits for that signal. `cfn-response` (which
//! every CDK custom-resource handler uses) does exactly this.
//!
//! fakecloud sent no `ResponseURL` at all, so handlers either threw on
//! `new URL(undefined)` or silently never signalled, and the provisioner marked
//! the resource complete regardless. This is the receiving end: an in-process
//! registry the internal HTTP route writes and the provisioner reads.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::RwLock;

/// A signal a handler PUT to its `ResponseURL`.
#[derive(Debug, Clone)]
pub struct CustomResourceSignal {
    pub status: String,
    pub reason: Option<String>,
    pub physical_resource_id: Option<String>,
}

impl CustomResourceSignal {
    pub fn failed(&self) -> bool {
        self.status.eq_ignore_ascii_case("FAILED")
    }

    /// The failure reason a stack event should carry.
    pub fn failure_reason(&self) -> String {
        self.reason
            .clone()
            .unwrap_or_else(|| "custom resource reported FAILED".to_string())
    }
}

/// Signals keyed by the event's `RequestId`, which is unique per invocation.
#[derive(Default)]
pub struct CustomResourceResponses {
    signals: RwLock<HashMap<String, CustomResourceSignal>>,
}

pub type SharedCustomResourceResponses = Arc<CustomResourceResponses>;

impl CustomResourceResponses {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a signal from a handler's PUT. Unknown or malformed bodies are
    /// rejected so a stray request cannot fail an unrelated resource.
    pub fn record(&self, request_id: &str, body: &serde_json::Value) -> bool {
        let Some(status) = body.get("Status").and_then(|v| v.as_str()) else {
            return false;
        };
        self.signals.write().insert(
            request_id.to_string(),
            CustomResourceSignal {
                status: status.to_string(),
                reason: body
                    .get("Reason")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                physical_resource_id: body
                    .get("PhysicalResourceId")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
            },
        );
        true
    }

    /// Wait up to `timeout` for the signal for `request_id`, removing it.
    ///
    /// The handler PUTs before returning, so a synchronous invoke has usually
    /// already produced the signal; the wait covers the window where the PUT is
    /// still in flight as the invoke response comes back. `None` means the
    /// handler never signalled -- an older handler that does not use
    /// `cfn-response` -- and the caller keeps its previous lenient behaviour
    /// rather than failing a resource that may well have worked.
    pub fn wait_for(&self, request_id: &str, timeout: Duration) -> Option<CustomResourceSignal> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(signal) = self.signals.write().remove(request_id) {
                return Some(signal);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Drop a signal that arrived for an invocation nobody is waiting on, so a
    /// long-lived server does not accumulate them.
    pub fn forget(&self, request_id: &str) {
        self.signals.write().remove(request_id);
    }
}

/// The `ResponseURL` for one invocation, given the base fakecloud is reachable
/// at *from inside a Lambda container*.
pub fn response_url(base: &str, request_id: &str) -> String {
    format!(
        "{}/_fakecloud/cfn/custom-resource-response/{request_id}",
        base.trim_end_matches('/')
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(status: &str, reason: Option<&str>) -> serde_json::Value {
        let mut m = serde_json::Map::new();
        m.insert("Status".into(), serde_json::json!(status));
        if let Some(r) = reason {
            m.insert("Reason".into(), serde_json::json!(r));
        }
        serde_json::Value::Object(m)
    }

    #[test]
    fn a_recorded_signal_is_returned_once() {
        let responses = CustomResourceResponses::new();
        assert!(responses.record("req-1", &body("SUCCESS", None)));

        let signal = responses
            .wait_for("req-1", Duration::from_millis(0))
            .expect("signal");
        assert!(!signal.failed());
        // Consumed: a second waiter must not see a stale signal.
        assert!(responses
            .wait_for("req-1", Duration::from_millis(0))
            .is_none());
    }

    #[test]
    fn a_failed_signal_carries_its_reason() {
        let responses = CustomResourceResponses::new();
        responses.record("req-2", &body("FAILED", Some("bucket not empty")));
        let signal = responses
            .wait_for("req-2", Duration::from_millis(0))
            .expect("signal");
        assert!(signal.failed());
        assert_eq!(signal.failure_reason(), "bucket not empty");
    }

    #[test]
    fn a_failed_signal_without_a_reason_still_reads_sensibly() {
        let responses = CustomResourceResponses::new();
        responses.record("req-3", &body("FAILED", None));
        let signal = responses.wait_for("req-3", Duration::ZERO).expect("signal");
        assert!(signal.failure_reason().contains("FAILED"));
    }

    #[test]
    fn a_body_without_a_status_is_rejected() {
        // A stray PUT must not be able to fail an unrelated resource.
        let responses = CustomResourceResponses::new();
        assert!(!responses.record("req-4", &serde_json::json!({"Reason": "nope"})));
        assert!(responses.wait_for("req-4", Duration::ZERO).is_none());
    }

    #[test]
    fn no_signal_within_the_timeout_is_not_a_failure() {
        let responses = CustomResourceResponses::new();
        assert!(responses
            .wait_for("never", Duration::from_millis(30))
            .is_none());
    }

    #[test]
    fn response_url_is_built_per_invocation() {
        assert_eq!(
            response_url("http://host.docker.internal:4566", "req-9"),
            "http://host.docker.internal:4566/_fakecloud/cfn/custom-resource-response/req-9"
        );
        // A trailing slash on the base must not double up.
        assert_eq!(
            response_url("http://127.0.0.1:4566/", "r"),
            "http://127.0.0.1:4566/_fakecloud/cfn/custom-resource-response/r"
        );
    }
}
