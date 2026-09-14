//! Shared container-to-host networking resolution for service runtimes
//! that spawn sibling containers (Lambda, ECS, RDS, ElastiCache).
//!
//! Captures the issue #1539 fix shape in one place so the four runtimes
//! that shell out to `docker`/`podman` can't drift apart again:
//!
//! - **podman** ships `host.containers.internal` as a built-in container
//!   DNS entry on every platform and must NOT receive
//!   `--add-host host.docker.internal:host-gateway` — rootless podman's
//!   gvproxy leaves the magic alias empty and the `create` fails with
//!   "host containers internal IP address is empty".
//! - **bare docker on Linux** has no `host-gateway` magic; the bridge
//!   gateway IP has to be resolved from the daemon and injected explicitly.
//! - **Docker Desktop on Mac/Windows** resolves the `host-gateway` magic
//!   value to the host's IP.
//! - when fakecloud itself runs in a container (`FAKECLOUD_IN_CONTAINER=1`,
//!   baked into the published image), the sibling containers it spawns
//!   publish their ports on the *host's* daemon — reachable from inside
//!   fakecloud's container as `host.docker.internal:<port>`, not
//!   `127.0.0.1:<port>`.

/// Actionable remediation appended to every error raised when a container
/// runtime (Docker/Podman) is required for an operation but none is
/// available. Kept in one place so RDS, Lambda, ECS, and the server startup
/// banner all surface the same fix steps and can't drift apart.
pub const CONTAINER_RUNTIME_HINT: &str = "Install and start Docker or Podman, or set FAKECLOUD_CONTAINER_CLI to your container CLI path.";

/// Auto-detect an available container CLI. Honors `FAKECLOUD_CONTAINER_CLI`
/// as an explicit override (returns `None` if the override doesn't work),
/// otherwise prefers `docker` then `podman`. Returns `None` when neither
/// is usable.
pub fn detect_container_cli() -> Option<String> {
    if let Ok(cli) = std::env::var("FAKECLOUD_CONTAINER_CLI") {
        return if cli_available(&cli) { Some(cli) } else { None };
    }
    if cli_available("docker") {
        Some("docker".to_string())
    } else if cli_available("podman") {
        Some("podman".to_string())
    } else {
        None
    }
}

/// How long to wait for `<cli> info` before giving up and treating the
/// runtime as unavailable. A healthy daemon answers in well under a second;
/// an unreachable or wedged daemon (stale `DOCKER_HOST`, Docker Desktop mid
/// start, a broken socket) can leave the CLI blocked on connect *forever*,
/// which would hang fakecloud startup and the test harness. Bounding the
/// probe turns "daemon wedged" into "no runtime detected" instead of a hang.
pub const CLI_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Process-global memo of `<cli> info` results, keyed by CLI name/path.
///
/// Container-runtime liveness is fixed for the life of a process, but every
/// service runtime (Lambda, ECS, RDS, ElastiCache, EC2, MQ, MSK, ...) probes
/// it independently at startup — a dozen-plus `detect_container_cli()` calls.
/// Without a memo each probe re-runs `docker info`; when the daemon is wedged
/// (see [`CLI_PROBE_TIMEOUT`]) those probes are serial 10s hangs that stack
/// into minutes, wedging server startup and the conformance `*_probe` tests.
/// Caching the first answer collapses that to a single probe.
static CLI_AVAILABLE_CACHE: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<String, bool>>,
> = std::sync::OnceLock::new();

/// True when the CLI responds to `<cli> info` with success within
/// [`CLI_PROBE_TIMEOUT`] — the same liveness probe every runtime used before
/// this module existed, but bounded so an unreachable daemon can't hang the
/// caller indefinitely (the CLI blocks on connect with no timeout of its own),
/// and memoized per process so a dozen runtimes probing at startup don't each
/// pay that bound.
pub fn cli_available(cli: &str) -> bool {
    let cache =
        CLI_AVAILABLE_CACHE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));
    if let Some(&cached) = cache.lock().unwrap().get(cli) {
        return cached;
    }
    let result = probe_cli(cli);
    cache.lock().unwrap().insert(cli.to_string(), result);
    result
}

/// Run the bounded `<cli> info` liveness probe once (uncached).
fn probe_cli(cli: &str) -> bool {
    let child = spawn_bounded(
        std::process::Command::new(cli)
            .arg("info")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null()),
    );
    let Ok(mut child) = child else {
        return false;
    };
    wait_bounded_group(&mut child) && child.wait().map(|s| s.success()).unwrap_or(false)
}

/// Spawn a container-CLI command in a process group of its own (Unix), so a
/// timed-out call can be torn down whole. `FAKECLOUD_CONTAINER_CLI` is
/// routinely a wrapper -- `sh -c 'exec docker "$@"'`, a `podman-remote` shim --
/// which makes the real command a *grandchild*: it survives `Child::kill`, goes
/// on holding whatever pipes we handed it, and keeps running against a wedged
/// daemon forever. Its own group makes it reachable by a single signal.
/// Detaching these from terminal job control is fine: their lifetime is managed
/// by deadline here, not by the shell fakecloud was started from.
fn spawn_bounded(cmd: &mut std::process::Command) -> std::io::Result<std::process::Child> {
    #[cfg(unix)]
    {
        std::os::unix::process::CommandExt::process_group(cmd, 0);
    }
    cmd.spawn()
}

/// How far a timed-out call's kill reaches.
enum KillScope {
    /// The direct child only -- the safe default for a child of unknown
    /// provenance, which may share fakecloud's own process group.
    Child,
    /// The child's whole process group, valid only for a child from
    /// [`spawn_bounded`], which put it in a group of its own.
    Group,
}

/// Wait for `child` up to [`CLI_PROBE_TIMEOUT`], killing it on expiry. Returns
/// whether it exited on its own. Every container-CLI call goes through this:
/// a liveness probe answering does not promise the next call will, and an
/// unbounded one blocks the caller rather than just that command.
///
/// Kills only the direct child, so it is safe for any child. Callers in this
/// module spawn through `spawn_bounded` and use `wait_bounded_group` instead,
/// which takes a wrapper CLI's grandchildren down too.
pub fn wait_bounded(child: &mut std::process::Child) -> bool {
    wait_bounded_scoped(child, KillScope::Child)
}

/// [`wait_bounded`] for a child from [`spawn_bounded`]: the expiry kill hits the
/// child's process group, so a wrapper CLI's grandchildren die with it.
fn wait_bounded_group(child: &mut std::process::Child) -> bool {
    wait_bounded_scoped(child, KillScope::Group)
}

fn wait_bounded_scoped(child: &mut std::process::Child, scope: KillScope) -> bool {
    let deadline = std::time::Instant::now() + CLI_PROBE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return true,
            Ok(None) => {}
            Err(_) => return false,
        }
        if std::time::Instant::now() >= deadline {
            // Daemon is wedged: kill the blocked call and report failure.
            kill_expired(child, &scope);
            let _ = child.wait();
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}

/// SIGKILL a timed-out child, and its process group when the caller vouches
/// that the group is ours ([`KillScope::Group`]). The group id is the child's
/// pid, and the child is still unreaped here, so the pid cannot have been
/// recycled and the signal cannot stray onto an unrelated group.
#[cfg(unix)]
fn kill_expired(child: &mut std::process::Child, scope: &KillScope) {
    if matches!(scope, KillScope::Group) {
        // SAFETY: `kill` with a negative pid targets the process group of that
        // id; any pid value is safe to pass.
        let _ = unsafe { libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL) };
    }
    let _ = child.kill();
}

/// Windows has no process-group signal (a job object would be needed), so the
/// direct child is as far as the kill reaches; [`run_bounded`] still bounds the
/// wait on its stdout reader so the caller can't be held by a surviving
/// grandchild.
#[cfg(not(unix))]
fn kill_expired(child: &mut std::process::Child, _scope: &KillScope) {
    let _ = child.kill();
}

/// Whether the stdout reader thread ended before the call returned.
#[derive(Debug)]
enum ReaderState {
    /// The reader returned; its thread is gone.
    Finished,
    /// The reader is still blocked on the pipe because a write end we could not
    /// close is held outside the child's process group. The thread outlives the
    /// call; the caller does not wait for it.
    Abandoned,
}

/// Floor on how long [`run_bounded`] waits for its stdout reader once the call
/// is over (it also gets whatever is left of the call's own budget). Both exits
/// close every write end we control -- the child exited, or its whole process
/// group was killed -- which ends the blocked `read_to_end` at once, so this
/// covers scheduling only. It exists so a write end held somewhere we cannot
/// reach costs the caller a few hundred milliseconds instead of blocking it for
/// good, which is what an unbounded join did.
const READER_DRAIN_GRACE: std::time::Duration = std::time::Duration::from_millis(500);

/// Run a container-CLI command and return its stdout, or `None` when it fails
/// or outruns [`CLI_PROBE_TIMEOUT`].
pub fn bounded_output(cli: &str, args: &[&str]) -> Option<String> {
    run_bounded(cli, args).0
}

/// [`bounded_output`], plus whether its stdout reader finished -- so the
/// timeout path's "no reader left behind" guarantee is unit-testable instead of
/// only observable as a thread that never goes away.
fn run_bounded(cli: &str, args: &[&str]) -> (Option<String>, ReaderState) {
    let deadline = std::time::Instant::now() + CLI_PROBE_TIMEOUT;
    let child = spawn_bounded(
        std::process::Command::new(cli)
            .args(args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null()),
    );
    let Ok(mut child) = child else {
        return (None, ReaderState::Finished);
    };
    let Some(mut stdout) = child.stdout.take() else {
        kill_expired(&mut child, &KillScope::Group);
        let _ = child.wait();
        return (None, ReaderState::Finished);
    };
    // Drain stdout while waiting. A child whose output outgrows the pipe
    // buffer blocks on write until someone reads it, so waiting for exit
    // first would deadlock until the deadline and then report the sweep as
    // failed -- `docker ps -a` across a busy host is exactly that much output.
    //
    // The channel doubles as the reader's "I'm done" signal: the send is the
    // last thing the thread does before dropping the pipe's read end, so a
    // received buffer proves no reader is parked behind us. A `JoinHandle`
    // can't say that without blocking, which on the timeout path is exactly
    // what we must not do.
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = std::io::Read::read_to_end(&mut stdout, &mut buf);
        let _ = tx.send(buf);
    });
    // On expiry `wait_bounded_group` has killed the whole process group, so a
    // wrapper CLI's grandchild releases the write end and the reader returns
    // instead of blocking for the life of the process -- one leaked thread per
    // call, on precisely the wedged-daemon path these bounds exist for.
    let exited = wait_bounded_group(&mut child);
    let status = child.wait().ok();
    // Whatever is left of the call's own budget, and never less than the grace:
    // a prompt call can afford to wait out a reader thread the scheduler hasn't
    // run yet, a timed-out one gets only the grace, and either way the caller is
    // back within CLI_PROBE_TIMEOUT plus that grace.
    let grace = deadline
        .saturating_duration_since(std::time::Instant::now())
        .max(READER_DRAIN_GRACE);
    let drained = rx.recv_timeout(grace).ok();
    let output = match (exited, status, &drained) {
        (true, Some(status), Some(buf)) if status.success() => {
            Some(String::from_utf8_lossy(buf).into_owned())
        }
        _ => None,
    };
    let reader = if drained.is_some() {
        ReaderState::Finished
    } else {
        ReaderState::Abandoned
    };
    (output, reader)
}

/// Run a container-CLI command for its effect only, bounded the same way.
/// Returns whether it succeeded.
pub fn bounded_status(cli: &str, args: &[String]) -> bool {
    let Ok(mut child) = spawn_bounded(
        std::process::Command::new(cli)
            .args(args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null()),
    ) else {
        return false;
    };
    wait_bounded_group(&mut child) && child.wait().map(|s| s.success()).unwrap_or(false)
}

/// True if the given PID is a live process on this host.
///
/// On Unix this is `kill(pid, 0)`: it returns 0 if the process exists
/// (including zombies), or sets `errno` to `ESRCH` if not. On non-Unix
/// platforms it conservatively returns `true`, so a caller never removes a
/// resource it can't prove is orphaned.
#[cfg(unix)]
pub fn pid_alive(pid: u32) -> bool {
    // SAFETY: `kill` with signal 0 is a liveness probe; it does not
    // actually deliver a signal. Any PID value is safe to pass.
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if rc == 0 {
        return true;
    }
    // errno == EPERM means the process exists but we can't signal it —
    // still alive from our perspective.
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
pub fn pid_alive(_pid: u32) -> bool {
    true
}

/// Whether a container or network labelled `fakecloud-instance=<label>` was
/// left behind by a fakecloud process that is gone. The label is
/// `fakecloud-<pid>`; an object is orphaned only when that PID is neither the
/// current process nor alive. Several fakecloud processes can share one
/// daemon (parallel test servers, side-by-side installs), so an object owned
/// by *another live* process is never an orphan. A label that doesn't parse is
/// not treated as an orphan either -- nothing proves its owner is gone.
pub fn owned_by_dead_process(label: &str, is_alive: impl Fn(u32) -> bool) -> bool {
    let Some(pid) = label
        .strip_prefix("fakecloud-")
        .and_then(|p| p.parse::<u32>().ok())
    else {
        return false;
    };
    pid != std::process::id() && !is_alive(pid)
}

/// True when `cli` is podman or a podman-compatible binary. Matches on the
/// filename component so absolute paths (`/opt/homebrew/bin/podman`) and
/// wrappers (`podman-remote`) both register as podman. Docker Desktop's
/// compatibility CLI is named `docker`, so this check is safe.
pub fn is_podman_binary(cli: &str) -> bool {
    std::path::Path::new(cli)
        .file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.contains("podman"))
        .unwrap_or(false)
}

/// Detect the Docker bridge gateway IP on Linux. Returns `None` if
/// detection fails (caller falls back to the conventional `172.17.0.1`).
///
/// Goes through [`bounded_output`] like every other container-CLI call here:
/// `network inspect` talks to the same daemon as the liveness probe, so a
/// wedged one blocks it on connect forever. This runs inside runtime
/// constructors on Linux, where an unbounded call hangs server startup outright
/// -- the exact failure [`CLI_PROBE_TIMEOUT`] exists to prevent. On timeout the
/// caller just takes the conventional fallback.
pub fn detect_bridge_gateway(cli: &str) -> Option<String> {
    let stdout = bounded_output(
        cli,
        &[
            "network",
            "inspect",
            "bridge",
            "--format",
            "{{range .IPAM.Config}}{{.Gateway}}{{end}}",
        ],
    )?;
    let gateway = stdout.trim().to_string();
    if gateway.is_empty() || !gateway.contains('.') {
        return None;
    }
    Some(gateway)
}

/// Resolved container-to-host networking for a given CLI. Built once at
/// runtime construction and reused for every container spawn.
#[derive(Debug, Clone)]
pub struct HostNetworking {
    /// DNS name a spawned container uses to reach fakecloud on the host.
    /// `host.containers.internal` for podman, `host.docker.internal` for
    /// docker.
    pub host_alias: String,
    /// `<alias>:<value>` argument for `--add-host`, injected into every
    /// container `create`/`run`. `None` when the runtime provides the
    /// alias natively (podman).
    pub add_host_arg: Option<String>,
    /// Address fakecloud uses to reach the *sibling* containers it just
    /// spawned (readiness probes + advertised endpoints). `127.0.0.1`
    /// when fakecloud runs on the host; `host.docker.internal` when
    /// fakecloud is itself containerized (`FAKECLOUD_IN_CONTAINER=1`).
    pub sibling_host: String,
}

impl HostNetworking {
    /// Resolve networking for `cli`, reading `FAKECLOUD_IN_CONTAINER` from
    /// the process environment.
    pub fn detect(cli: &str) -> Self {
        let (host_alias, mut add_host_arg) = resolve_host_alias(cli);
        // A resolving `host.docker.internal` is only trustworthy evidence that
        // the runtime provides the alias natively (and will inject it into
        // sibling containers too) when fakecloud is itself containerized:
        // Docker-Desktop-class runtimes inject the alias into CONTAINERS, never
        // onto the host. On a bare native-Linux host a resolving alias is
        // spurious (a hijacking NXDOMAIN resolver, a stray /etc/hosts entry, or
        // a wildcard search domain), so suppressing the bridge --add-host there
        // would break the host route sibling containers need. Gate the
        // suppression on the in-container signal to avoid that regression.
        let in_container = in_container_mode(std::env::var("FAKECLOUD_IN_CONTAINER").ok());
        add_host_arg = preserve_native_host_alias(
            add_host_arg,
            in_container && host_alias_resolves(&host_alias),
        );
        let sibling_host =
            resolve_sibling_host(&host_alias, std::env::var("FAKECLOUD_IN_CONTAINER").ok());
        Self {
            host_alias,
            add_host_arg,
            sibling_host,
        }
    }

    /// Convenience: append the `--add-host <alias>:<value>` flag pair to a
    /// growing argv vector when this runtime needs an explicit mapping.
    /// No-op for podman.
    pub fn push_add_host_args(&self, argv: &mut Vec<String>) {
        if let Some(arg) = &self.add_host_arg {
            argv.push("--add-host".to_string());
            argv.push(arg.clone());
        }
    }
}

/// How long to wait for the blocking `getaddrinfo` in [`host_alias_resolves`]
/// before giving up and returning `false`. `getaddrinfo` has no timeout of its
/// own, and a slow or unreachable DNS server would otherwise block a runtime
/// thread at startup (this runs inside runtime constructors under
/// `#[tokio::main]`). Bounding it — same tradeoff as [`CLI_PROBE_TIMEOUT`] —
/// turns "DNS wedged" into "alias doesn't resolve", the safe default that keeps
/// the `--add-host` bridge mapping.
pub const HOST_ALIAS_RESOLVE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// True when `host_alias` resolves via the process resolver. The `getaddrinfo`
/// call is blocking with no timeout of its own, so it runs on a spawned thread
/// bounded by [`HOST_ALIAS_RESOLVE_TIMEOUT`]; on timeout we return `false` (the
/// safe default that keeps `--add-host`). A leaked resolver thread on timeout
/// is acceptable — same tradeoff as [`probe_cli`].
fn host_alias_resolves(host_alias: &str) -> bool {
    let (tx, rx) = std::sync::mpsc::channel();
    let alias = host_alias.to_string();
    std::thread::spawn(move || {
        let resolves = std::net::ToSocketAddrs::to_socket_addrs(&(alias.as_str(), 0)).is_ok();
        let _ = tx.send(resolves);
    });
    rx.recv_timeout(HOST_ALIAS_RESOLVE_TIMEOUT).unwrap_or(false)
}

fn preserve_native_host_alias(
    add_host_arg: Option<String>,
    should_suppress: bool,
) -> Option<String> {
    if add_host_arg.is_some() && should_suppress {
        // Suppress the injected `--add-host host.docker.internal:<vm-bridge-ip>`
        // only when fakecloud is containerized AND the alias already resolves
        // (see the gate in `detect`). In that case a Docker-Desktop-class
        // runtime provides `host.docker.internal` natively inside every sibling
        // container, pointing at the real host; injecting the VM bridge-gateway
        // IP would shadow it and break the host route. On a bare host — where a
        // hijacking resolver can make the alias resolve spuriously — the caller
        // passes `false` here so native Linux docker keeps the bridge mapping
        // it genuinely needs.
        None
    } else {
        add_host_arg
    }
}

/// Compute the `(host_alias, add_host_arg)` pair for a CLI. Pure except
/// for the bridge-gateway daemon probe on Linux docker, so the macOS /
/// podman branches are unit-testable without a daemon.
pub fn resolve_host_alias(cli: &str) -> (String, Option<String>) {
    if is_podman_binary(cli) {
        // Podman provides `host.containers.internal` natively on every
        // supported platform; injecting `host-gateway` on macOS fails
        // because rootless podman's gvproxy doesn't expose the magic alias.
        ("host.containers.internal".to_string(), None)
    } else if cfg!(target_os = "linux") {
        // Bare docker on Linux: resolve the bridge gateway IP and add an
        // explicit alias. `host.docker.internal:host-gateway` only works
        // on Docker Desktop; native Linux docker has no such magic.
        let ip = detect_bridge_gateway(cli).unwrap_or_else(|| "172.17.0.1".to_string());
        (
            "host.docker.internal".to_string(),
            Some(format!("host.docker.internal:{ip}")),
        )
    } else {
        // Docker Desktop on Mac/Windows: `host-gateway` is the magic alias
        // that resolves to the host's IP.
        (
            "host.docker.internal".to_string(),
            Some("host.docker.internal:host-gateway".to_string()),
        )
    }
}

/// Decide what address fakecloud uses to reach the sibling containers it
/// just spawned. Pure helper so the env-var parsing can be tested without
/// touching the process's real environment.
///
/// - `Some("1")` / `Some("true")` (case-insensitive) -> fakecloud is in a
///   container; the siblings publish their ports on the host's daemon and
///   are reachable at the same host alias the spawned containers use to
///   reach fakecloud — `host.docker.internal` under docker,
///   `host.containers.internal` under podman. Hardcoding
///   `host.docker.internal` here broke podman, whose gvproxy network only
///   resolves `host.containers.internal` (issue #1539 follow-up).
/// - anything else, including `None` -> fakecloud runs on the host,
///   siblings live on `127.0.0.1:<port>`.
pub fn resolve_sibling_host(host_alias: &str, env_value: Option<String>) -> String {
    if in_container_mode(env_value) {
        host_alias.to_string()
    } else {
        "127.0.0.1".to_string()
    }
}

/// Parse the `FAKECLOUD_IN_CONTAINER` signal: `Some("1")` or a case-insensitive
/// `Some("true")` mean fakecloud is running inside a container; anything else,
/// including `None`, means it runs on the host. Single source of truth for the
/// parse so `detect`'s native-alias gate and `resolve_sibling_host` can't drift.
fn in_container_mode(env_value: Option<String>) -> bool {
    env_value
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// Hostnames fakecloud's bundled ECR/OCI registry can be addressed from a
/// sibling container or the host, each at `server_port`.
///
/// A container-spawning service rewrites the image pull URI to the runtime's
/// sibling host -- `host.docker.internal` under Docker, `host.containers.internal`
/// under podman -- or leaves it `localhost` / `127.0.0.1` when fakecloud runs on
/// the host (`localhost:<port>` is the documented local ECR endpoint, e.g.
/// `localhost:4566`). The registry enforces auth, and the Docker/Podman CLI only
/// attaches the `Authorization` header for hosts present in `config.json`, so the
/// isolated pull config must list *every* alias or the pull gets a 401. The map
/// previously omitted the podman alias, so image-based Lambda/ECS pulls failed
/// under podman-in-a-container (bug-audit 2026-06-20, 0.B2). Authorize all of
/// them with the same credential; centralized here so the two builders can't
/// drift again.
pub fn registry_auth_hosts(server_port: u16) -> Vec<String> {
    [
        "localhost",
        "127.0.0.1",
        "host.docker.internal",
        "host.containers.internal",
    ]
    .iter()
    .map(|host| format!("{host}:{server_port}"))
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_available_false_for_missing_binary() {
        // A binary that doesn't exist fails to spawn -> unavailable, fast.
        assert!(!cli_available("definitely-not-a-real-cli-binary-xyz-123"));
    }

    #[cfg(unix)]
    #[test]
    fn cli_available_bounds_a_hanging_probe() {
        // A CLI whose `info` invocation blocks forever (like `docker info`
        // against an unreachable daemon) must not hang the caller: the probe
        // is killed at CLI_PROBE_TIMEOUT and reported unavailable. Regression
        // test for the local-conformance-probe hang.
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("fc-clitest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("hangcli");
        std::fs::write(&script, "#!/bin/sh\nsleep 600\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::io::stdout().flush().ok();

        let start = std::time::Instant::now();
        let available = cli_available(script.to_str().unwrap());
        let elapsed = start.elapsed();

        std::fs::remove_dir_all(&dir).ok();
        assert!(!available, "a hanging probe must report unavailable");
        assert!(
            elapsed < CLI_PROBE_TIMEOUT + std::time::Duration::from_secs(5),
            "probe took {elapsed:?}, expected it bounded near {CLI_PROBE_TIMEOUT:?}"
        );
    }

    #[test]
    fn is_podman_binary_matches_bare_name() {
        assert!(is_podman_binary("podman"));
        assert!(is_podman_binary("podman-remote"));
    }

    #[test]
    fn registry_auth_hosts_includes_podman_alias() {
        // The podman sibling alias (host.containers.internal) must be authorized
        // or image-based Lambda/ECS pulls 401 under podman-in-a-container (0.B2).
        let hosts = registry_auth_hosts(4566);
        assert!(hosts.contains(&"localhost:4566".to_string()));
        assert!(hosts.contains(&"127.0.0.1:4566".to_string()));
        assert!(hosts.contains(&"host.docker.internal:4566".to_string()));
        assert!(
            hosts.contains(&"host.containers.internal:4566".to_string()),
            "podman sibling alias must be authorized: {hosts:?}"
        );
    }

    #[test]
    fn is_podman_binary_matches_absolute_path() {
        assert!(is_podman_binary("/opt/homebrew/bin/podman"));
        assert!(is_podman_binary("/usr/local/bin/podman-remote"));
    }

    #[test]
    fn is_podman_binary_rejects_docker() {
        assert!(!is_podman_binary("docker"));
        assert!(!is_podman_binary("/usr/local/bin/docker"));
        assert!(!is_podman_binary("docker-credential-helper"));
    }

    #[test]
    fn resolve_host_alias_podman_has_no_add_host() {
        let (alias, add_host) = resolve_host_alias("podman");
        assert_eq!(alias, "host.containers.internal");
        assert_eq!(add_host, None);
        let (alias, add_host) = resolve_host_alias("/opt/homebrew/bin/podman");
        assert_eq!(alias, "host.containers.internal");
        assert_eq!(add_host, None);
    }

    #[test]
    fn resolve_host_alias_docker_emits_add_host() {
        let (alias, add_host) = resolve_host_alias("docker");
        assert_eq!(alias, "host.docker.internal");
        // On macOS this is host-gateway; on Linux it's a bridge IP. Either
        // way docker must get an explicit --add-host.
        assert!(add_host.is_some());
        assert!(add_host.unwrap().starts_with("host.docker.internal:"));
    }

    #[test]
    fn native_host_alias_prevents_docker_add_host_override() {
        let add_host =
            preserve_native_host_alias(Some("host.docker.internal:host-gateway".to_string()), true);

        assert_eq!(add_host, None);
    }

    #[test]
    fn unresolved_host_alias_keeps_docker_add_host() {
        let add_host = preserve_native_host_alias(
            Some("host.docker.internal:host-gateway".to_string()),
            false,
        );

        assert_eq!(
            add_host.as_deref(),
            Some("host.docker.internal:host-gateway")
        );
    }

    #[test]
    fn absent_docker_add_host_remains_absent() {
        assert_eq!(preserve_native_host_alias(None, true), None);
        assert_eq!(preserve_native_host_alias(None, false), None);
    }

    #[test]
    fn in_container_mode_parses_truthy_values() {
        assert!(in_container_mode(Some("1".to_string())));
        assert!(in_container_mode(Some("true".to_string())));
        assert!(in_container_mode(Some("True".to_string())));
        assert!(in_container_mode(Some("TRUE".to_string())));
    }

    #[test]
    fn in_container_mode_rejects_falsey_and_absent() {
        assert!(!in_container_mode(None));
        assert!(!in_container_mode(Some(String::new())));
        assert!(!in_container_mode(Some("0".to_string())));
        assert!(!in_container_mode(Some("false".to_string())));
        assert!(!in_container_mode(Some("yes".to_string())));
    }

    #[test]
    fn native_alias_gate_suppresses_only_in_container() {
        // The gate `detect` computes: `in_container && host_alias_resolves`.
        let add_host = || Some("host.docker.internal:172.17.0.1".to_string());

        // In-container + resolves -> Desktop-class runtime provides the alias
        // natively in siblings; drop the shadowing bridge mapping.
        let in_container = true;
        let resolves = true;
        assert_eq!(
            preserve_native_host_alias(add_host(), in_container && resolves),
            None,
        );

        // NOT in-container (bare host) + resolves -> the resolving alias is
        // spurious (hijacking resolver / stray hosts entry). Native Linux docker
        // needs the bridge mapping; must NOT drop it. Regression guard.
        let in_container = false;
        let resolves = true;
        assert_eq!(
            preserve_native_host_alias(add_host(), in_container && resolves).as_deref(),
            Some("host.docker.internal:172.17.0.1"),
        );

        // In-container + does NOT resolve -> nothing native to preserve; keep
        // the injected mapping.
        let in_container = true;
        let resolves = false;
        assert_eq!(
            preserve_native_host_alias(add_host(), in_container && resolves).as_deref(),
            Some("host.docker.internal:172.17.0.1"),
        );
    }

    #[test]
    fn resolve_sibling_host_defaults_to_loopback() {
        assert_eq!(
            resolve_sibling_host("host.docker.internal", None),
            "127.0.0.1"
        );
        assert_eq!(
            resolve_sibling_host("host.docker.internal", Some(String::new())),
            "127.0.0.1"
        );
        assert_eq!(
            resolve_sibling_host("host.docker.internal", Some("0".to_string())),
            "127.0.0.1"
        );
        assert_eq!(
            resolve_sibling_host("host.containers.internal", Some("false".to_string())),
            "127.0.0.1"
        );
    }

    #[test]
    fn resolve_sibling_host_uses_host_alias_when_in_container() {
        // Docker: siblings reachable at host.docker.internal.
        assert_eq!(
            resolve_sibling_host("host.docker.internal", Some("1".to_string())),
            "host.docker.internal"
        );
        assert_eq!(
            resolve_sibling_host("host.docker.internal", Some("true".to_string())),
            "host.docker.internal"
        );
        assert_eq!(
            resolve_sibling_host("host.docker.internal", Some("TRUE".to_string())),
            "host.docker.internal"
        );
        // Podman: must use host.containers.internal, NOT host.docker.internal
        // (issue #1539 follow-up — gvproxy only resolves the containers alias).
        assert_eq!(
            resolve_sibling_host("host.containers.internal", Some("1".to_string())),
            "host.containers.internal"
        );
    }

    #[test]
    fn detect_wires_sibling_host_to_podman_alias_in_container() {
        // Full path: a podman binary in a container must advertise siblings
        // at host.containers.internal. resolve_host_alias drives host_alias,
        // which resolve_sibling_host then reuses.
        let (alias, add_host) = resolve_host_alias("podman");
        assert_eq!(alias, "host.containers.internal");
        assert_eq!(add_host, None);
        assert_eq!(
            resolve_sibling_host(&alias, Some("1".to_string())),
            "host.containers.internal"
        );
    }

    #[test]
    fn only_objects_of_a_dead_owner_are_orphans() {
        let me = std::process::id();
        let alive = |pid: u32| pid == 4242;
        // Another live fakecloud process: never an orphan.
        assert!(!owned_by_dead_process("fakecloud-4242", alive));
        // Its owner is gone: an orphan.
        assert!(owned_by_dead_process("fakecloud-777", alive));
        // The current process, even if the probe says otherwise.
        assert!(!owned_by_dead_process(&format!("fakecloud-{me}"), |_| {
            false
        }));
        // Nothing proves an unparseable owner is gone.
        for label in ["", "fakecloud-", "fakecloud-abc", "other-777"] {
            assert!(!owned_by_dead_process(label, alive), "{label:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn pid_alive_probes_real_processes() {
        assert!(pid_alive(std::process::id()));
        assert!(!pid_alive(u32::MAX - 1));
    }

    #[test]
    fn push_add_host_args_noop_for_podman() {
        let net = HostNetworking {
            host_alias: "host.containers.internal".to_string(),
            add_host_arg: None,
            sibling_host: "127.0.0.1".to_string(),
        };
        let mut argv = vec!["create".to_string()];
        net.push_add_host_args(&mut argv);
        assert_eq!(argv, vec!["create".to_string()]);
    }

    #[test]
    fn push_add_host_args_emits_for_docker() {
        let net = HostNetworking {
            host_alias: "host.docker.internal".to_string(),
            add_host_arg: Some("host.docker.internal:host-gateway".to_string()),
            sibling_host: "127.0.0.1".to_string(),
        };
        let mut argv = vec!["create".to_string()];
        net.push_add_host_args(&mut argv);
        assert_eq!(
            argv,
            vec![
                "create".to_string(),
                "--add-host".to_string(),
                "host.docker.internal:host-gateway".to_string(),
            ]
        );
    }
}

#[cfg(test)]
mod bounded_cli_tests {
    use super::*;

    /// A wedged daemon leaves the CLI blocked on connect forever. Every
    /// container call has to end at the bound instead of hanging its caller,
    /// which for the reaper means hanging server startup.
    #[test]
    fn a_hanging_cli_call_is_cut_off() {
        let start = std::time::Instant::now();
        let mut child = std::process::Command::new("sleep")
            .arg("600")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("sleep is available");
        assert!(!wait_bounded(&mut child));
        assert!(
            start.elapsed() < CLI_PROBE_TIMEOUT + std::time::Duration::from_secs(5),
            "the wait must end at the bound"
        );
    }

    /// Output larger than a pipe buffer (64 KiB on Linux) must come back
    /// whole. Waiting for the child to exit before reading blocks it on write
    /// forever, so this used to burn the full timeout and report failure.
    #[test]
    fn output_larger_than_the_pipe_buffer_still_comes_back() {
        let start = std::time::Instant::now();
        // 200_000 bytes: comfortably past the buffer on every supported host.
        let out = bounded_output("sh", &["-c", "printf 'x%.0s' $(seq 1 200000)"])
            .expect("a large but prompt call must succeed");
        assert_eq!(out.len(), 200_000, "output was truncated");
        assert!(
            start.elapsed() < CLI_PROBE_TIMEOUT,
            "a prompt call must not reach the deadline"
        );
    }

    #[test]
    fn a_prompt_cli_call_returns_its_output() {
        assert_eq!(
            bounded_output("echo", &["abc123"])
                .as_deref()
                .map(str::trim),
            Some("abc123")
        );
        assert!(bounded_status("true", &[]));
        assert!(!bounded_status("false", &[]));
    }

    /// The happy path must still hand back the child's output *and* collect the
    /// reader, so the no-leak guarantee isn't bought by dropping output.
    #[test]
    fn a_prompt_cli_call_collects_its_reader() {
        let (output, reader) = run_bounded("echo", &["abc123"]);
        assert_eq!(output.as_deref().map(str::trim), Some("abc123"));
        assert!(
            matches!(reader, ReaderState::Finished),
            "reader was {reader:?}, expected it collected"
        );
    }

    /// The bridge-gateway probe talks to the same daemon as the liveness probe,
    /// so it has to end at the same bound. It used to be a plain
    /// `Command::output()`, which against a wedged daemon hung whichever runtime
    /// constructor called it -- on Linux, every container-backed service at
    /// server startup. Timing out is not an error here: the caller falls back to
    /// the conventional `172.17.0.1`.
    #[cfg(unix)]
    #[test]
    fn a_hanging_bridge_gateway_probe_is_cut_off() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("fc-gwtest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("hangcli");
        std::fs::write(&script, "#!/bin/sh\nsleep 600\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let start = std::time::Instant::now();
        let gateway = detect_bridge_gateway(script.to_str().unwrap());
        let elapsed = start.elapsed();

        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(gateway, None, "a wedged daemon must report no gateway");
        assert!(
            elapsed < CLI_PROBE_TIMEOUT + READER_DRAIN_GRACE + std::time::Duration::from_secs(5),
            "the probe took {elapsed:?}, expected it bounded near {CLI_PROBE_TIMEOUT:?}"
        );
    }

    /// The unavailable-CLI path keeps its shape: nothing to spawn, no gateway,
    /// and the caller's fallback stands.
    #[test]
    fn a_missing_cli_reports_no_bridge_gateway() {
        assert_eq!(
            detect_bridge_gateway("definitely-not-a-real-cli-binary-xyz-123"),
            None
        );
    }

    /// A CLI that succeeds with no output -- an `inspect --format` over a bridge
    /// with no IPAM config -- still means "no gateway", not an empty
    /// `--add-host` value. Unchanged by the bounding; guarded so it stays that
    /// way.
    #[test]
    fn an_empty_gateway_is_rejected() {
        assert_eq!(detect_bridge_gateway("true"), None);
    }

    /// `FAKECLOUD_CONTAINER_CLI` is routinely a wrapper (`sh -c 'exec docker
    /// "$@"'`, a `podman-remote` shim), which makes the real command a
    /// grandchild holding the stdout pipe. Killing only the direct child left
    /// the reader's `read_to_end` blocked forever -- a thread parked for the
    /// life of the process, once per call, on exactly the wedged-daemon path
    /// these bounds were added for (the server reaper calls this at startup).
    /// The reader reporting in is the evidence: EOF on that pipe is only
    /// possible once every write end is closed, so a collected buffer proves
    /// the grandchildren went down with the call.
    #[cfg(unix)]
    #[test]
    fn a_timed_out_wrapper_call_leaves_no_reader_behind() {
        let start = std::time::Instant::now();
        // A wrapper that outlives its own kill: the backgrounded sleep inherits
        // the stdout pipe and is not the process we spawned.
        let (output, reader) = run_bounded("sh", &["-c", "sleep 600 & sleep 600"]);
        assert_eq!(output, None, "a wedged call must report failure");
        assert!(
            matches!(reader, ReaderState::Finished),
            "reader was {reader:?}: the stdout reader must not outlive the call"
        );
        assert!(
            start.elapsed()
                < CLI_PROBE_TIMEOUT + READER_DRAIN_GRACE + std::time::Duration::from_secs(5),
            "the call must still end at the bound, took {:?}",
            start.elapsed()
        );
    }
}
