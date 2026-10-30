//! The service scheduler's throttle on tasks that keep failing to start.
//!
//! Amazon ECS: "When tasks in a service fail to transition from `PENDING` to `RUNNING` state and
//! instead move directly to `STOPPED`, the scheduler incrementally increases the time between
//! restart attempts", up to a maximum of 27 minutes, and "generates a service event message":
//! `(service <name>) is unable to consistently start tasks successfully.` It retries indefinitely,
//! and a new task definition "immediately returns the service to normal, non-throttled operation".
//! Tasks that stop after reaching `RUNNING` (a crashing container, a failed load balancer health
//! check) do not count
//! (<https://docs.aws.amazon.com/AmazonECS/latest/developerguide/service-throttle-logic.md>,
//! fetched 2026-10-21).
//!
//! AWS does not document the increments or how many failures it takes to throttle, so those are
//! choices here: the first retry is immediate, then the wait doubles from [`BASE_DELAY`] up to the
//! documented [`MAX_DELAY`].

use chrono::{DateTime, Duration, Utc};
use std::collections::BTreeMap;

use crate::state::{Service, ServiceEvent, Task, SERVICE_EVENT_LIMIT};

/// The documented ceiling, "which may change in future updates".
pub const MAX_DELAY: Duration = Duration::minutes(27);
/// The wait after the second consecutive failure; each further failure doubles it.
pub const BASE_DELAY: Duration = Duration::seconds(30);

/// How long to wait before the next launch after `failures` launches in a row failed to start.
/// `None` means retry at the next scheduler tick: a single failure is not yet "consistent".
pub fn delay_after(failures: u32) -> Option<Duration> {
    if failures < 2 {
        return None;
    }
    // 2^20 * 30s is far past the cap already; the clamp keeps the shift from overflowing.
    let doublings = (failures - 2).min(20);
    let millis = BASE_DELAY
        .num_milliseconds()
        .saturating_mul(1i64 << doublings);
    Some(Duration::milliseconds(millis).min(MAX_DELAY))
}

/// When `task` reached `RUNNING`, if it ever did.
fn started_at(task: &Task) -> Option<DateTime<Utc>> {
    task.started_at
}

/// When `task` failed to start: it is `STOPPED` and never ran.
fn failed_to_start_at(task: &Task) -> Option<DateTime<Utc>> {
    (task.last_status == "STOPPED" && task.started_at.is_none())
        .then(|| task.stopped_at.unwrap_or(task.created_at))
}

/// Count the tasks of `service` that started or failed to start since the last call, and set or
/// clear the throttle. Returns `true` when the throttle has just been activated.
///
/// Counting by time rather than by remembering which tasks were launched covers every way a task
/// can be launched (CreateService, UpdateService, the scheduler) without each having to register
/// its tasks here.
pub fn settle(service: &mut Service, tasks: &BTreeMap<String, Task>, now: DateTime<Utc>) -> bool {
    let tag = format!("ecs-svc/{}", service.service_name);
    let since = service.launch_throttle.counted_until;
    let is_new = |at: DateTime<Utc>| since.is_none_or(|mark| at > mark);
    let mut failures = 0u32;
    let mut started = false;
    for task in tasks.values().filter(|t| {
        t.started_by.as_deref() == Some(tag.as_str()) && t.cluster_name == service.cluster_name
    }) {
        if started_at(task).is_some_and(is_new) {
            started = true;
        }
        if failed_to_start_at(task).is_some_and(is_new) {
            failures += 1;
        }
    }
    let throttle = &mut service.launch_throttle;
    let was_throttled = throttle.not_before.is_some();
    throttle.counted_until = Some(now);
    if started {
        // The service is starting tasks again: back to normal.
        throttle.consecutive_failures = 0;
        throttle.not_before = None;
    }
    if failures == 0 {
        return false;
    }
    throttle.consecutive_failures = throttle.consecutive_failures.saturating_add(failures);
    throttle.not_before = delay_after(throttle.consecutive_failures).map(|d| now + d);
    let activated = throttle.not_before.is_some() && !was_throttled;
    if activated {
        let message = format!(
            "(service {}) is unable to consistently start tasks successfully.",
            service.service_name
        );
        service.events.insert(
            0,
            ServiceEvent {
                id: uuid::Uuid::new_v4().to_string(),
                created_at: now,
                message,
            },
        );
        service.events.truncate(SERVICE_EVENT_LIMIT);
    }
    activated
}

/// Whether `service` must not launch tasks yet.
pub fn is_throttled(service: &Service, now: DateTime<Utc>) -> bool {
    service
        .launch_throttle
        .not_before
        .is_some_and(|until| now < until)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A task of the `web` service, `ago` seconds old, in `status`; `ran` says whether it ever
    /// reached `RUNNING` (a start time), which is what separates a crash from a failure to start.
    fn task(id: &str, status: &str, ran: bool, now: DateTime<Utc>) -> Task {
        let mut t = crate::state::tests_support::task(id);
        t.started_by = Some("ecs-svc/web".to_string());
        t.last_status = status.to_string();
        t.created_at = now - Duration::seconds(5);
        t.started_at = ran.then(|| now - Duration::seconds(3));
        t.stopped_at = (status == "STOPPED").then(|| now - Duration::seconds(1));
        t
    }

    fn service() -> Service {
        crate::state::tests_support::service("web")
    }

    fn tasks(list: Vec<Task>) -> BTreeMap<String, Task> {
        list.into_iter().map(|t| (t.task_id.clone(), t)).collect()
    }

    #[test]
    fn the_first_retry_is_immediate_then_the_wait_doubles_to_the_documented_cap() {
        assert_eq!(delay_after(0), None);
        assert_eq!(delay_after(1), None);
        assert_eq!(delay_after(2), Some(Duration::seconds(30)));
        assert_eq!(delay_after(3), Some(Duration::seconds(60)));
        assert_eq!(delay_after(4), Some(Duration::seconds(120)));
        assert_eq!(delay_after(6), Some(Duration::minutes(8)));
        // 30s * 2^6 is 32 minutes: capped at 27.
        assert_eq!(delay_after(8), Some(Duration::minutes(27)));
        assert_eq!(delay_after(9), Some(Duration::minutes(27)));
        // A service that has failed for days must not overflow the shift.
        assert_eq!(delay_after(u32::MAX), Some(Duration::minutes(27)));
    }

    #[test]
    fn one_failure_does_not_throttle_but_the_second_does_and_says_so_once() {
        let now = Utc::now();
        let mut svc = service();

        let t = tasks(vec![task("t1", "STOPPED", false, now)]);
        assert!(!settle(&mut svc, &t, now));
        assert_eq!(svc.launch_throttle.consecutive_failures, 1);
        assert!(!is_throttled(&svc, now));
        assert!(svc.events.is_empty());

        // A second task fails after the first count.
        let later = now + Duration::seconds(10);
        let t = tasks(vec![
            task("t1", "STOPPED", false, now),
            task("t2", "STOPPED", false, later),
        ]);
        assert!(settle(&mut svc, &t, later));
        assert_eq!(svc.launch_throttle.consecutive_failures, 2);
        assert!(is_throttled(&svc, later));
        assert!(!is_throttled(&svc, later + Duration::seconds(31)));
        assert_eq!(svc.events.len(), 1);
        assert_eq!(
            svc.events[0].message,
            "(service web) is unable to consistently start tasks successfully."
        );

        // The next failure lengthens the wait but is not a new throttle, so no second event.
        let after = later + Duration::seconds(31);
        let t = tasks(vec![
            task("t1", "STOPPED", false, now),
            task("t2", "STOPPED", false, later),
            task("t3", "STOPPED", false, after),
        ]);
        assert!(!settle(&mut svc, &t, after));
        assert_eq!(svc.events.len(), 1);
        assert!(is_throttled(&svc, after + Duration::seconds(59)));
        assert!(!is_throttled(&svc, after + Duration::seconds(61)));
    }

    #[test]
    fn a_failure_is_counted_once_however_many_ticks_see_it() {
        let now = Utc::now();
        let mut svc = service();
        let t = tasks(vec![task("t1", "STOPPED", false, now)]);
        settle(&mut svc, &t, now);
        settle(&mut svc, &t, now + Duration::seconds(3));
        settle(&mut svc, &t, now + Duration::seconds(6));
        assert_eq!(svc.launch_throttle.consecutive_failures, 1);
    }

    #[test]
    fn a_task_that_reaches_running_clears_the_throttle_even_if_it_stops_later() {
        let now = Utc::now();
        let mut svc = service();
        svc.launch_throttle.consecutive_failures = 5;
        svc.launch_throttle.not_before = Some(now + Duration::minutes(5));

        // It ran and then crashed: STOPPED, but with a start time. Not a failure to start.
        let t = tasks(vec![task("t1", "STOPPED", true, now)]);
        assert!(!settle(&mut svc, &t, now));
        assert_eq!(svc.launch_throttle.consecutive_failures, 0);
        assert!(svc.launch_throttle.not_before.is_none());
    }

    #[test]
    fn a_task_still_pending_is_not_counted_either_way() {
        let now = Utc::now();
        let mut svc = service();
        let t = tasks(vec![task("t1", "PENDING", false, now)]);
        assert!(!settle(&mut svc, &t, now));
        assert_eq!(svc.launch_throttle.consecutive_failures, 0);
    }

    #[test]
    fn another_services_tasks_are_not_counted() {
        let now = Utc::now();
        let mut svc = service();
        let mut other = task("t1", "STOPPED", false, now);
        other.started_by = Some("ecs-svc/api".to_string());
        assert!(!settle(&mut svc, &tasks(vec![other]), now));
        assert_eq!(svc.launch_throttle.consecutive_failures, 0);
    }

    #[test]
    fn events_are_newest_first_and_capped_at_what_aws_keeps() {
        let now = Utc::now();
        let mut svc = service();
        for n in 0..(SERVICE_EVENT_LIMIT + 5) {
            svc.events.insert(
                0,
                ServiceEvent {
                    id: n.to_string(),
                    created_at: now,
                    message: format!("event {n}"),
                },
            );
        }
        let t = tasks(vec![
            task("a", "STOPPED", false, now),
            task("b", "STOPPED", false, now),
        ]);
        assert!(settle(&mut svc, &t, now));
        assert_eq!(svc.events.len(), SERVICE_EVENT_LIMIT);
        assert!(svc.events[0]
            .message
            .contains("unable to consistently start tasks"));
    }
}
