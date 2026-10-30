//! Rolling deployments for services with the `ECS` deployment controller.
//!
//! When a service's task definition changes, "the Amazon ECS service scheduler replaces the
//! currently running tasks with new tasks", bounded by two limits: `minimumHealthyPercent` is "the
//! lower limit on the number of tasks that should be running and healthy ... as a percent of the
//! desired number of tasks" (rounded up), and `maximumPercent` is "the upper limit on the number of
//! tasks that should be running" (rounded down)
//! (<https://docs.aws.amazon.com/AmazonECS/latest/developerguide/deployment-type-ecs.md>, fetched
//! 2026-10-21). For a service on the `ECS` controller the console defaults are 100 and 200.
//!
//! The plan is computed from the tasks that exist, not from how the task definition was changed, so
//! it covers `UpdateService` and a CloudFormation update alike: the latter used to relabel the
//! deployments and leave the old tasks running for ever, because nothing noticed they were on an old
//! revision.

use chrono::{DateTime, Utc};
use std::collections::BTreeMap;

use crate::state::{Service, Task};

/// What the scheduler should do for one service this tick.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Plan {
    /// Tasks to start on the service's current task definition.
    pub launch: i32,
    /// Tasks on an older task definition to stop, oldest first.
    pub stop: Vec<String>,
}

fn belongs_to(task: &Task, service: &Service) -> bool {
    task.started_by.as_deref() == Some(format!("ecs-svc/{}", service.service_name).as_str())
        && task.cluster_name == service.cluster_name
}

/// Occupying capacity: on its way up or running, whatever its desired status.
fn is_live(task: &Task) -> bool {
    matches!(
        task.last_status.as_str(),
        "RUNNING" | "PENDING" | "PROVISIONING"
    )
}

/// Wanted to be there: live and not marked for stopping.
fn is_wanted(task: &Task) -> bool {
    is_live(task) && task.desired_status == "RUNNING"
}

fn is_pending(task: &Task) -> bool {
    matches!(task.last_status.as_str(), "PENDING" | "PROVISIONING")
}

fn is_serving(task: &Task) -> bool {
    task.last_status == "RUNNING" && task.desired_status == "RUNNING"
}

/// Percent of `desired`, rounded up (`minimumHealthyPercent`) or down (`maximumPercent`).
fn percent_of(desired: i32, percent: i32, round_up: bool) -> i32 {
    let product = i64::from(desired.max(0)) * i64::from(percent.max(0));
    let value = if round_up {
        (product + 99) / 100
    } else {
        product / 100
    };
    value.min(i64::from(i32::MAX)) as i32
}

/// Work out the launches and stops that move `service` towards its current task definition without
/// leaving the limits.
pub fn plan(service: &Service, tasks: &BTreeMap<String, Task>) -> Plan {
    let mine: Vec<&Task> = tasks.values().filter(|t| belongs_to(t, service)).collect();
    let desired = service.desired_count.max(0);
    let min_healthy = percent_of(
        desired,
        service.minimum_healthy_percent.unwrap_or(100),
        true,
    );
    let max_total = percent_of(desired, service.maximum_percent.unwrap_or(200), false).max(desired);
    let is_new = |t: &&&Task| t.task_definition_arn == service.task_definition_arn;

    let new_wanted = mine.iter().filter(is_new).filter(|t| is_wanted(t)).count() as i32;
    let new_serving = mine.iter().filter(is_new).filter(|t| is_serving(t)).count() as i32;
    let mut old: Vec<&Task> = mine
        .iter()
        .copied()
        .filter(|t| t.task_definition_arn != service.task_definition_arn && is_wanted(t))
        .collect();
    old.sort_by_key(|t| t.created_at);
    let old_serving = old.iter().filter(|t| is_serving(t)).count() as i32;

    let mut stop: Vec<String> = Vec::new();
    // Old tasks that are not serving yet cost nothing to stop.
    stop.extend(
        old.iter()
            .filter(|t| is_pending(t))
            .map(|t| t.task_id.clone()),
    );
    // Serving ones can go only while enough stay up.
    let stoppable = (new_serving + old_serving - min_healthy)
        .max(0)
        .min(old_serving) as usize;
    stop.extend(
        old.iter()
            .filter(|t| is_serving(t))
            .take(stoppable)
            .map(|t| t.task_id.clone()),
    );

    // Tasks already being stopped still occupy capacity until they are STOPPED.
    let live = mine.iter().filter(|t| is_live(t)).count() as i32;
    let launch = (desired - new_wanted).min(max_total - live).max(0);
    Plan { launch, stop }
}

/// Recompute the counts the scheduler used to only ever add to: each deployment's running and
/// pending tasks (the tasks on its task definition), an old deployment's desired count (what is left
/// of it), the primary's rollout state, and drop an old deployment once it has no tasks left.
pub fn refresh(service: &mut Service, tasks: &BTreeMap<String, Task>, now: DateTime<Utc>) {
    if service.deployment_controller != "ECS" {
        return;
    }
    let mine: Vec<&Task> = tasks.values().filter(|t| belongs_to(t, service)).collect();
    let count = |td: &str, pending: bool| {
        mine.iter()
            .filter(|t| t.task_definition_arn == td && t.desired_status == "RUNNING")
            .filter(|t| {
                if pending {
                    is_pending(t)
                } else {
                    t.last_status == "RUNNING"
                }
            })
            .count() as i32
    };
    service.running_count = mine.iter().filter(|t| is_serving(t)).count() as i32;
    service.pending_count = mine
        .iter()
        .filter(|t| t.desired_status == "RUNNING" && is_pending(t))
        .count() as i32;

    let primary_td = service.task_definition_arn.clone();
    let old_live = mine
        .iter()
        .filter(|t| t.task_definition_arn != primary_td && is_wanted(t))
        .count();
    for d in service.deployments.iter_mut() {
        let (running, pending) = (
            count(&d.task_definition_arn, false),
            count(&d.task_definition_arn, true),
        );
        if (d.running_count, d.pending_count) != (running, pending) {
            d.running_count = running;
            d.pending_count = pending;
            d.updated_at = now;
        }
        if d.status == "ACTIVE" {
            d.desired_count = running + pending;
        } else if d.status == "PRIMARY"
            && d.pending_hook_id.is_none()
            && matches!(d.rollout_state.as_str(), "IN_PROGRESS" | "COMPLETED")
        {
            let done = running == d.desired_count && old_live == 0;
            let state = if done { "COMPLETED" } else { "IN_PROGRESS" };
            if d.rollout_state != state {
                d.rollout_state = state.to_string();
                d.updated_at = now;
            }
        }
    }
    // An old deployment with nothing left in it is finished.
    service
        .deployments
        .retain(|d| d.status != "ACTIVE" || d.running_count + d.pending_count > 0);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::tests_support;

    const OLD: &str = "arn:aws:ecs:us-east-1:000000000000:task-definition/web:1";
    const NEW: &str = "arn:aws:ecs:us-east-1:000000000000:task-definition/web:2";

    fn service(desired: i32, min: Option<i32>, max: Option<i32>) -> Service {
        let mut s = tests_support::service("web");
        s.task_definition_arn = NEW.to_string();
        s.desired_count = desired;
        s.minimum_healthy_percent = min;
        s.maximum_percent = max;
        s
    }

    /// A task of `web` on `td`, `age` seconds old, in `status` (desired RUNNING unless stopping).
    fn task(id: &str, td: &str, status: &str, age: i64) -> Task {
        let mut t = tests_support::task(id);
        t.started_by = Some("ecs-svc/web".to_string());
        t.task_definition_arn = td.to_string();
        t.last_status = status.to_string();
        t.created_at = Utc::now() - chrono::Duration::seconds(age);
        t
    }

    fn map(list: Vec<Task>) -> BTreeMap<String, Task> {
        list.into_iter().map(|t| (t.task_id.clone(), t)).collect()
    }

    #[test]
    fn a_service_on_its_current_task_definition_just_tops_up_to_desired() {
        let s = service(3, None, None);
        let t = map(vec![task("a", NEW, "RUNNING", 9)]);
        assert_eq!(
            plan(&s, &t),
            Plan {
                launch: 2,
                stop: vec![]
            }
        );
        assert_eq!(
            plan(&s, &BTreeMap::new()),
            Plan {
                launch: 3,
                stop: vec![]
            }
        );
    }

    #[test]
    fn a_new_task_definition_starts_a_replacement_beside_the_old_task_and_keeps_it_serving() {
        // The case that was stuck for ever: one old task running, nothing on the new revision.
        let s = service(1, None, None);
        let t = map(vec![task("old", OLD, "RUNNING", 100)]);
        // Default maximum 200% allows one extra; default minimum 100% forbids stopping the old one yet.
        assert_eq!(
            plan(&s, &t),
            Plan {
                launch: 1,
                stop: vec![]
            }
        );
    }

    #[test]
    fn the_old_task_is_stopped_once_the_new_one_is_serving() {
        let s = service(1, None, None);
        let t = map(vec![
            task("old", OLD, "RUNNING", 100),
            task("new", NEW, "RUNNING", 5),
        ]);
        assert_eq!(
            plan(&s, &t),
            Plan {
                launch: 0,
                stop: vec!["old".into()]
            }
        );
    }

    #[test]
    fn nothing_is_stopped_while_the_replacement_is_still_starting() {
        let s = service(1, None, None);
        let t = map(vec![
            task("old", OLD, "RUNNING", 100),
            task("new", NEW, "PENDING", 5),
        ]);
        assert_eq!(plan(&s, &t), Plan::default());
    }

    #[test]
    fn minimum_healthy_percent_lets_old_tasks_go_before_replacements_serve() {
        // 4 desired, 50% minimum: two old tasks may be stopped straight away.
        let s = service(4, Some(50), Some(100));
        let t = map(vec![
            task("o1", OLD, "RUNNING", 400),
            task("o2", OLD, "RUNNING", 300),
            task("o3", OLD, "RUNNING", 200),
            task("o4", OLD, "RUNNING", 100),
        ]);
        let p = plan(&s, &t);
        assert_eq!(
            p.stop,
            vec!["o1".to_string(), "o2".to_string()],
            "oldest first, two of four"
        );
        // Maximum 100%: no room to start anything until they are gone.
        assert_eq!(p.launch, 0);
    }

    #[test]
    fn maximum_percent_bounds_the_replacements_started_beside_the_old_tasks() {
        // 4 desired, 150% maximum: six tasks at most, so two replacements beside four old ones.
        let s = service(4, Some(100), Some(150));
        let t = map(vec![
            task("o1", OLD, "RUNNING", 400),
            task("o2", OLD, "RUNNING", 300),
            task("o3", OLD, "RUNNING", 200),
            task("o4", OLD, "RUNNING", 100),
        ]);
        assert_eq!(plan(&s, &t).launch, 2);
    }

    #[test]
    fn old_tasks_that_are_not_serving_yet_are_stopped_at_once() {
        let s = service(1, None, None);
        let t = map(vec![
            task("old", OLD, "PENDING", 100),
            task("new", NEW, "PENDING", 5),
        ]);
        assert_eq!(plan(&s, &t).stop, vec!["old".to_string()]);
    }

    #[test]
    fn tasks_already_being_stopped_are_not_stopped_again_but_still_hold_capacity() {
        let s = service(1, None, Some(100));
        let mut draining = task("old", OLD, "RUNNING", 100);
        draining.desired_status = "STOPPED".to_string();
        let t = map(vec![draining]);
        // It is still RUNNING, so with a 100% maximum there is no room for the replacement yet.
        assert_eq!(plan(&s, &t), Plan::default());
    }

    #[test]
    fn another_services_tasks_are_ignored() {
        let s = service(1, None, None);
        let mut other = task("x", OLD, "RUNNING", 100);
        other.started_by = Some("ecs-svc/api".to_string());
        assert_eq!(
            plan(&s, &map(vec![other])),
            Plan {
                launch: 1,
                stop: vec![]
            }
        );
    }

    #[test]
    fn percentages_round_the_way_aws_documents() {
        assert_eq!(percent_of(3, 50, true), 2, "minimum rounds up");
        assert_eq!(percent_of(3, 50, false), 1, "maximum rounds down");
        assert_eq!(percent_of(0, 200, false), 0);
        assert_eq!(percent_of(1, 200, false), 2);
        assert_eq!(percent_of(i32::MAX, 200, false), i32::MAX);
    }

    #[test]
    fn refresh_counts_each_deployment_from_its_own_tasks_and_finishes_the_old_one() {
        let now = Utc::now();
        let mut s = service(1, None, None);
        s.deployments = vec![
            crate::state::Deployment::new_primary(NEW, 1, "FARGATE", &[]),
            {
                let mut d = crate::state::Deployment::new_primary(OLD, 1, "FARGATE", &[]);
                d.status = "ACTIVE".to_string();
                d.pending_count = 2467; // the value that only ever grew
                d
            },
        ];
        let t = map(vec![
            task("old", OLD, "RUNNING", 100),
            task("new", NEW, "PENDING", 5),
        ]);
        refresh(&mut s, &t, now);
        let primary = &s.deployments[0];
        let active = &s.deployments[1];
        assert_eq!((primary.running_count, primary.pending_count), (0, 1));
        assert_eq!(
            (active.running_count, active.pending_count),
            (1, 0),
            "no longer 2467"
        );
        assert_eq!(active.desired_count, 1);
        assert_eq!(primary.rollout_state, "IN_PROGRESS");
        assert_eq!((s.running_count, s.pending_count), (1, 1));

        // The replacement serves and the old task is gone: the old deployment ends, the rollout completes.
        let t = map(vec![task("new", NEW, "RUNNING", 5)]);
        refresh(&mut s, &t, now);
        assert_eq!(s.deployments.len(), 1);
        assert_eq!(s.deployments[0].status, "PRIMARY");
        assert_eq!(s.deployments[0].rollout_state, "COMPLETED");
    }

    #[test]
    fn refresh_leaves_other_deployment_controllers_alone() {
        let mut s = service(1, None, None);
        s.deployment_controller = "CODE_DEPLOY".to_string();
        s.deployments = vec![crate::state::Deployment::new_primary(
            NEW,
            1,
            "FARGATE",
            &[],
        )];
        s.deployments[0].pending_count = 7;
        refresh(&mut s, &BTreeMap::new(), Utc::now());
        assert_eq!(s.deployments[0].pending_count, 7);
    }

    #[test]
    fn refresh_does_not_overwrite_a_failed_rollout() {
        let mut s = service(1, None, None);
        s.deployments = vec![crate::state::Deployment::new_primary(
            NEW,
            1,
            "FARGATE",
            &[],
        )];
        s.deployments[0].rollout_state = "FAILED".to_string();
        refresh(
            &mut s,
            &map(vec![task("new", NEW, "RUNNING", 5)]),
            Utc::now(),
        );
        assert_eq!(s.deployments[0].rollout_state, "FAILED");
    }
}
