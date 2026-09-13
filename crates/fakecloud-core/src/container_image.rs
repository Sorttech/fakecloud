//! Image pulls for the runtimes that launch user-supplied images (ECS, and
//! Batch through it; Lambda `PackageType=Image` functions).
//!
//! A bare `docker pull` always contacts the registry, even when the image is
//! already in the local cache, so any registry hiccup fails the launch. The
//! common one is rate limiting: anonymous pulls from `public.ecr.aws` are
//! capped per source IP, and a burst of task launches -- or several processes
//! sharing one NAT address -- gets `429 Too Many Requests` back.
//!
//! The ECS container agent handles this the way this module does. Under its
//! default `ECS_IMAGE_PULL_BEHAVIOR`, a failed pull falls back to the image
//! cached on the instance, and pulls are retried with backoff before giving
//! up. A pull that fails with nothing cached still fails, with the registry's
//! own error.

use std::path::Path;
use std::time::Duration;

use tokio::process::Command;

/// Pull attempts made while the registry keeps rate limiting, before the
/// last error is returned.
const MAX_PULL_ATTEMPTS: u32 = 5;

/// Delay before the first retry; doubles on each further retry.
const BASE_RETRY_DELAY: Duration = Duration::from_secs(1);

/// How an image became available for a launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PulledImage {
    /// The registry served the image.
    Pulled,
    /// The pull failed but the image was already cached locally, so the
    /// cached copy is used. Carries the pull's error for logging.
    Cached { pull_error: String },
}

/// Pull `reference` with the container `cli`, falling back to a locally
/// cached copy when the pull fails and retrying while the registry is rate
/// limiting. `docker_config` is exported as `DOCKER_CONFIG` for the pull so
/// registry credentials resolve.
///
/// Returns the pull's stderr as the error when the image is neither pullable
/// nor cached.
pub async fn pull_image(
    cli: &str,
    docker_config: Option<&Path>,
    reference: &str,
) -> Result<PulledImage, String> {
    pull_image_with(cli, docker_config, reference, BASE_RETRY_DELAY).await
}

async fn pull_image_with(
    cli: &str,
    docker_config: Option<&Path>,
    reference: &str,
    base_delay: Duration,
) -> Result<PulledImage, String> {
    let mut delay = base_delay;
    let mut attempt = 1;
    loop {
        let mut cmd = Command::new(cli);
        if let Some(p) = docker_config {
            cmd.env("DOCKER_CONFIG", p);
        }
        let out = cmd
            .args(["pull", reference])
            .output()
            .await
            .map_err(|e| format!("{cli} pull: {e}"))?;
        if out.status.success() {
            return Ok(PulledImage::Pulled);
        }
        let pull_error = String::from_utf8_lossy(&out.stderr).trim().to_string();

        if image_cached(cli, reference).await {
            tracing::warn!(
                image = %reference,
                error = %pull_error,
                "image pull failed; using the locally cached image"
            );
            return Ok(PulledImage::Cached { pull_error });
        }
        if attempt >= MAX_PULL_ATTEMPTS || !is_rate_limited(&pull_error) {
            return Err(pull_error);
        }
        tracing::info!(
            image = %reference,
            attempt,
            retry_in_ms = delay.as_millis() as u64,
            "image pull rate limited; retrying"
        );
        tokio::time::sleep(delay).await;
        delay *= 2;
        attempt += 1;
    }
}

/// Whether `reference` resolves to an image in the local cache.
async fn image_cached(cli: &str, reference: &str) -> bool {
    Command::new(cli)
        .args(["image", "inspect", reference])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Whether a pull error is the registry throttling the caller. Docker reports
/// the status line (`429 Too Many Requests`) or the registry error code
/// (`toomanyrequests`); ECR Public's throttle message is `Rate exceeded`.
fn is_rate_limited(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    lower.contains("toomanyrequests")
        || lower.contains("too many requests")
        || lower.contains("rate exceeded")
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// A stand-in container CLI. `pull` fails with `pull_stderr` for the first
    /// `pull_failures` calls and succeeds after; `image inspect` succeeds only
    /// when `cached`. Every invocation is appended to `calls.log`.
    struct FakeCli {
        dir: tempfile::TempDir,
    }

    impl FakeCli {
        fn new(pull_failures: u32, pull_stderr: &str, cached: bool) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let script = format!(
                r#"#!/bin/sh
d="{dir}"
echo "$*" >> "$d/calls.log"
case "$1" in
  pull)
    n=$(cat "$d/pulls" 2>/dev/null || echo 0)
    n=$((n + 1))
    echo "$n" > "$d/pulls"
    if [ "$n" -le {pull_failures} ]; then
      echo '{pull_stderr}' >&2
      exit 1
    fi
    exit 0 ;;
  image)
    [ "{cached}" = "true" ] && exit 0
    echo 'Error: No such image' >&2
    exit 1 ;;
esac
exit 2
"#,
                dir = dir.path().display(),
            );
            let path = dir.path().join("cli");
            std::fs::write(&path, script).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            Self { dir }
        }

        fn cli(&self) -> String {
            self.dir.path().join("cli").display().to_string()
        }

        fn calls(&self) -> Vec<String> {
            std::fs::read_to_string(self.dir.path().join("calls.log"))
                .unwrap_or_default()
                .lines()
                .map(String::from)
                .collect()
        }

        async fn pull(&self) -> Result<PulledImage, String> {
            pull_image_with(&self.cli(), None, "alpine:3.20", Duration::from_millis(1)).await
        }
    }

    const THROTTLED: &str = "Error response from daemon: unexpected status from HEAD request to https://public.ecr.aws/v2/docker/library/alpine/manifests/3.20: 429 Too Many Requests";

    #[tokio::test]
    async fn a_successful_pull_needs_no_cache_check() {
        let cli = FakeCli::new(0, "", false);
        assert_eq!(cli.pull().await, Ok(PulledImage::Pulled));
        assert_eq!(cli.calls(), ["pull alpine:3.20"]);
    }

    #[tokio::test]
    async fn a_failed_pull_uses_the_cached_image() {
        let cli = FakeCli::new(u32::MAX, THROTTLED, true);
        let got = cli.pull().await;
        assert_eq!(
            got,
            Ok(PulledImage::Cached {
                pull_error: THROTTLED.to_string()
            })
        );
        assert_eq!(
            cli.calls(),
            ["pull alpine:3.20", "image inspect alpine:3.20"],
            "a cached image is used at once, without retrying the pull"
        );
    }

    #[tokio::test]
    async fn a_rate_limited_pull_with_nothing_cached_is_retried() {
        let cli = FakeCli::new(2, THROTTLED, false);
        assert_eq!(cli.pull().await, Ok(PulledImage::Pulled));
        let pulls = cli.calls().iter().filter(|c| c.starts_with("pull")).count();
        assert_eq!(pulls, 3);
    }

    #[tokio::test]
    async fn retries_stop_after_the_attempt_cap() {
        let cli = FakeCli::new(u32::MAX, THROTTLED, false);
        assert_eq!(cli.pull().await, Err(THROTTLED.to_string()));
        let pulls = cli.calls().iter().filter(|c| c.starts_with("pull")).count();
        assert_eq!(pulls, MAX_PULL_ATTEMPTS as usize);
    }

    #[tokio::test]
    async fn a_missing_image_fails_without_retrying() {
        let missing =
            "Error response from daemon: manifest for alpine:nope not found: manifest unknown";
        let cli = FakeCli::new(u32::MAX, missing, false);
        assert_eq!(cli.pull().await, Err(missing.to_string()));
        let pulls = cli.calls().iter().filter(|c| c.starts_with("pull")).count();
        assert_eq!(pulls, 1, "only a throttled pull is worth retrying");
    }

    #[test]
    fn rate_limit_detection_covers_each_registry_form() {
        assert!(is_rate_limited(THROTTLED));
        assert!(is_rate_limited(
            "toomanyrequests: You have reached your pull rate limit."
        ));
        assert!(is_rate_limited("Error: Rate exceeded"));
        assert!(!is_rate_limited("manifest unknown"));
        assert!(!is_rate_limited("pull access denied for foo"));
    }
}
