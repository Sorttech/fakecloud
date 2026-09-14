//! Startup reaper for orphaned backing containers.
//!
//! fakecloud spawns docker containers for RDS (postgres), ElastiCache (redis),
//! Lambda (runtime images), EC2 and ECS tasks, and labels each one with
//! `fakecloud-instance=fakecloud-<server-pid>`. ECS awsvpc per-task networks
//! carry the same label. Normal shutdown runs `stop_all()` on each runtime,
//! but if the server was killed with SIGKILL (or crashed, or OOM'd) those
//! containers (and networks) outlive the process and pile up.
//!
//! On startup we list every container — then every network — carrying the
//! `fakecloud-instance` label, parse the owning PID out of the label value,
//! and remove any whose owner is no longer alive. Objects owned by the
//! currently-running fakecloud process are always skipped.

/// Reap orphaned fakecloud-owned containers whose server PID is no longer alive.
///
/// Uses the same CLI detection policy as the runtimes: honors
/// `FAKECLOUD_CONTAINER_CLI` if set, otherwise tries `docker` then `podman`.
/// If no container CLI is available this is a silent no-op — fakecloud is
/// expected to start fine without docker.
pub fn reap_stale_containers() {
    // Uses the shared, *bounded+memoized* detection in `container_net`: an
    // unreachable/wedged daemon leaves a raw `docker info` blocked on connect
    // forever, and this reaper runs at startup, so a naive probe here would
    // hang the server (and every conformance `*_probe` test) indefinitely.
    let Some(cli) = fakecloud_core::container_net::detect_container_cli() else {
        return;
    };

    let reaped = reap_orphans(&cli, &["ps", "-a"], |id| {
        vec!["rm".to_string(), "-f".to_string(), id.to_string()]
    });
    if reaped > 0 {
        tracing::info!(count = reaped, "reaped orphaned backing containers");
    }

    // ECS awsvpc per-task networks carry the same ownership label. The
    // network driver refuses removal while a container is still attached,
    // so prune networks *after* containers. `network rm` is a no-op for an
    // already-gone network, so a partial container reap above doesn't wedge
    // this pass.
    let reaped_networks = reap_orphans(&cli, &["network", "ls"], |id| {
        vec!["network".to_string(), "rm".to_string(), id.to_string()]
    });
    if reaped_networks > 0 {
        tracing::info!(count = reaped_networks, "reaped orphaned backing networks");
    }
}

/// List objects carrying the `fakecloud-instance` label via
/// `<cli> <list_args> --filter label=fakecloud-instance`, then run the
/// `remove_argv(id)` command for every object whose owning PID is no longer
/// alive (skipping the current process and live owners). Returns the number
/// removed. Shared by the container and network reap passes.
fn reap_orphans(cli: &str, list_args: &[&str], remove_argv: impl Fn(&str) -> Vec<String>) -> usize {
    let mut args: Vec<&str> = list_args.to_vec();
    args.extend_from_slice(&[
        "--filter",
        "label=fakecloud-instance",
        "--format",
        "{{.ID}} {{.Label \"fakecloud-instance\"}}",
    ]);

    // Bounded: the liveness probe answering does not promise this call will,
    // and the reap runs synchronously before the server starts serving, so an
    // unbounded call here wedges startup rather than just the sweep.
    let Some(listing) = fakecloud_core::container_net::bounded_output(cli, &args) else {
        return 0;
    };

    let mut reaped = 0usize;

    for line in listing.lines() {
        let Some((id, label)) = line.split_once(' ') else {
            continue;
        };
        if !fakecloud_core::container_net::owned_by_dead_process(
            label,
            fakecloud_core::container_net::pid_alive,
        ) {
            continue;
        }
        let removed = fakecloud_core::container_net::bounded_status(cli, &remove_argv(id));
        if removed {
            reaped += 1;
        }
    }

    reaped
}

#[cfg(all(test, unix))]
mod tests {
    use fakecloud_core::container_net::pid_alive;

    #[test]
    fn self_is_alive() {
        assert!(pid_alive(std::process::id()));
    }

    #[test]
    fn init_is_alive() {
        assert!(pid_alive(1));
    }

    #[test]
    fn huge_pid_is_dead() {
        // Max u32 is far outside any reasonable PID range on any OS.
        assert!(!pid_alive(u32::MAX - 1));
    }
}
