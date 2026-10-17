//! Fake container CLIs for unit tests, shared by every crate that drives
//! `docker` / `podman` (enable the `test-support` feature from a
//! `[dev-dependencies]` entry to use it outside this crate).

use std::path::Path;
use std::time::{Duration, Instant};

/// Write `script` to `path` as an executable and run it once (with the single
/// argument `warmup`), so it is never `ETXTBSY` when a test runs it.
///
/// Executing a file another process holds open for writing fails with
/// `ETXTBSY`. Tests run on parallel threads, and a child forked by another test
/// while `fs::write` had the script open inherits that descriptor until it
/// execs. Running it once, retrying until nothing holds it, guarantees no
/// process can hold a writable descriptor to it afterwards. The warmup run's
/// own effects are the script's business: a fake that logs its calls should
/// ignore (or exit early on) `warmup`.
#[cfg(unix)]
pub fn write_executable(path: &Path, script: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, script).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut attempts = 0;
    loop {
        match std::process::Command::new(path).arg("warmup").output() {
            Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy && attempts < 200 => {
                attempts += 1;
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(e) => panic!("fake CLI {} did not run: {e}", path.display()),
            Ok(_) => return,
        }
    }
}

/// An executable `#!/bin/sh` fake CLI named `name` with `body`, in a fresh
/// temp dir. Returns the dir (keep it alive) and the CLI's path.
#[cfg(unix)]
pub fn fake_cli(name: &str, body: &str) -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(name);
    write_executable(&path, &format!("#!/bin/sh\n{body}"));
    let cli = path.display().to_string();
    (dir, cli)
}

/// Poll `cond` every 20ms until it holds or `timeout` passes; returns whether
/// it held.
pub fn wait_until(timeout: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if cond() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Read the pid a fake CLI wrote to `pidfile`, waiting up to 5s for it to
/// appear. Panics, naming the file, if it never does.
pub fn read_pidfile(pidfile: &Path) -> u32 {
    assert!(
        wait_until(Duration::from_secs(5), || std::fs::read_to_string(pidfile)
            .is_ok_and(|s| s.trim().parse::<u32>().is_ok())),
        "the fake CLI never wrote a pid to {}",
        pidfile.display()
    );
    std::fs::read_to_string(pidfile)
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

/// Whether `pid` is gone within 5s. A killed process reparented to init is
/// reaped promptly, so a `true` here means it was really killed.
pub fn pid_gone(pid: u32) -> bool {
    wait_until(Duration::from_secs(5), || {
        !crate::container_net::pid_alive(pid)
    })
}

/// SIGKILL `pid`, for a test cleaning up after a fake that is meant to
/// outlive it.
#[cfg(unix)]
pub fn kill_pid(pid: u32) {
    // SAFETY: signalling a pid; any value is safe to pass.
    let _ = unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
}
