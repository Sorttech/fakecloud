//! Image pulls for the runtimes that launch user-supplied images (ECS, and
//! Batch through it; Lambda `PackageType=Image` functions).
//!
//! A bare `docker pull` always contacts the registry, even when the image is
//! already in the local cache, so a momentary registry failure fails the
//! launch. The common one is rate limiting: anonymous pulls from
//! `public.ecr.aws` are capped per source IP, and a burst of task launches --
//! or several processes sharing one NAT address -- gets
//! `429 Too Many Requests` back.
//!
//! A transient failure (throttling, a registry 5xx, a network timeout) is
//! retried with backoff, and falls back to the image already cached locally
//! instead of failing the launch. A refusal is final: an image that no longer
//! exists or a pull the registry denies fails at once with the registry's own
//! error, even when a stale copy is cached. Otherwise an image deleted from
//! ECR, or one a repository policy denies, would keep launching from the
//! local copy -- which neither Fargate nor Lambda, having no per-host image
//! cache, ever does.

use std::path::Path;
use std::time::Duration;

use tokio::process::Command;

/// Pull attempts made while the registry keeps failing transiently, before
/// the last error is returned.
const MAX_PULL_ATTEMPTS: u32 = 5;

/// Delay before the first retry; doubles on each further retry.
const BASE_RETRY_DELAY: Duration = Duration::from_secs(1);

/// How an image became available for a launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PulledImage {
    /// The registry served the image.
    Pulled,
    /// The pull failed transiently but the image was already cached locally,
    /// so the cached copy is used. Carries the pull's error for logging.
    Cached { pull_error: String },
}

/// Pull `reference` with the container `cli`. A transient registry failure
/// falls back to a locally cached copy, or is retried with backoff when
/// nothing is cached; any other failure is returned at once. `docker_config`
/// is exported as `DOCKER_CONFIG` for the pull so registry credentials
/// resolve.
///
/// Returns the pull's stderr as the error.
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
        if !is_transient(&pull_error, reference) {
            return Err(pull_error);
        }
        if image_cached(cli, docker_config, reference).await {
            tracing::warn!(
                image = %reference,
                error = %pull_error,
                "image pull failed transiently; using the locally cached image"
            );
            return Ok(PulledImage::Cached { pull_error });
        }
        if attempt >= MAX_PULL_ATTEMPTS {
            return Err(pull_error);
        }
        tracing::info!(
            image = %reference,
            attempt,
            retry_in_ms = delay.as_millis() as u64,
            "image pull failed transiently; retrying"
        );
        tokio::time::sleep(delay).await;
        delay *= 2;
        attempt += 1;
    }
}

/// Whether `reference` resolves to an image in the local cache. Runs with
/// the same `DOCKER_CONFIG` as the pull: the config also selects the Docker
/// context, so without it the check could consult a different daemon than
/// the one that pulls and later runs the image.
async fn image_cached(cli: &str, docker_config: Option<&Path>, reference: &str) -> bool {
    let mut cmd = Command::new(cli);
    if let Some(p) = docker_config {
        cmd.env("DOCKER_CONFIG", p);
    }
    cmd.args(["image", "inspect", reference])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Whether a pull error is one a later attempt could succeed past: the
/// registry throttling the caller (the `429 Too Many Requests` status line,
/// the `toomanyrequests` error code, ECR Public's `Rate exceeded`), any 5xx
/// from the registry, or the network timing out or dropping the connection.
///
/// A refusal (not found, access denied, unauthorized) is never transient, and
/// wins over transient wording in the same message. Both are matched only
/// after the image's own name is removed from the message: the name is chosen
/// by the user and quoted in the error, so a repository spelled like either
/// kind of marker (`toomanyrequests/app`, `acme/access-denied-page`) must not
/// flip the classification.
fn is_transient(stderr: &str, reference: &str) -> bool {
    const REFUSED: [&str; 6] = [
        "manifest unknown",
        "not found",
        "denied",
        "unauthorized",
        "forbidden",
        "does not exist",
    ];
    const TRANSIENT: [&str; 8] = [
        "toomanyrequests",
        "too many requests",
        "rate exceeded",
        "i/o timeout",
        "tls handshake timeout",
        "connection reset by peer",
        "context deadline exceeded",
        "request canceled while waiting for connection",
    ];
    let message = without_image_name(&stderr.to_ascii_lowercase(), reference);
    if REFUSED.iter().any(|m| message.contains(m)) {
        return false;
    }
    TRANSIENT.iter().any(|m| message.contains(m)) || has_server_error_status(&message)
}

/// `message` (lowercase) with each way an error can quote the image blanked
/// out: the reference as given, and every trailing path of its repository
/// (`public.ecr.aws/acme/app`, `acme/app`, `app`) -- registries put the
/// repository path in URLs, and Podman expands short names to
/// `docker.io/library/<name>`. Only whole names are removed, bounded by
/// characters a name cannot contain, so a short repository like `d` never
/// cuts letters out of the surrounding words.
fn without_image_name(message: &str, reference: &str) -> String {
    let reference = reference.to_ascii_lowercase();
    let untagged = reference.split('@').next().unwrap_or(&reference);
    // A `:` after the last `/` starts the tag; one before it is a registry port.
    let repository = match (untagged.rfind(':'), untagged.rfind('/')) {
        (Some(colon), Some(slash)) if colon < slash => untagged,
        (Some(colon), _) => &untagged[..colon],
        (None, _) => untagged,
    };
    let mut names = vec![reference.as_str(), repository];
    names.extend(
        repository
            .match_indices('/')
            .map(|(i, _)| &repository[i + 1..]),
    );
    names.sort_by_key(|n| std::cmp::Reverse(n.len()));

    let mut out = message.to_string();
    for name in names.into_iter().filter(|n| !n.is_empty()) {
        out = remove_whole(&out, name);
    }
    out
}

/// `haystack` with every occurrence of `name` that is not part of a longer
/// name (flanked by a letter, digit, `.`, `_` or `-`) replaced by a space.
fn remove_whole(haystack: &str, name: &str) -> String {
    let is_name_char = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-');
    let mut out = String::with_capacity(haystack.len());
    let mut rest = haystack;
    while let Some(i) = rest.find(name) {
        let end = i + name.len();
        let before = rest[..i].chars().next_back();
        let after = rest[end..].chars().next();
        if before.is_some_and(is_name_char) || after.is_some_and(is_name_char) {
            out.push_str(&rest[..end]);
        } else {
            out.push_str(&rest[..i]);
            out.push(' ');
        }
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

/// Whether the message carries a 5xx HTTP status. Docker and Podman quote the
/// status as `: 503 Service Unavailable`, `status: 500`, or
/// `status code 502`; a three-digit number in that position from 500 to 599
/// counts, whatever reason phrase follows.
fn has_server_error_status(message: &str) -> bool {
    let bytes = message.as_bytes();
    ["status code ", "status: ", "status ", ": "]
        .iter()
        .flat_map(|prefix| message.match_indices(prefix).map(|(i, p)| i + p.len()))
        .any(|start| {
            let code = &bytes[start..bytes.len().min(start + 3)];
            code.len() == 3
                && code[0] == b'5'
                && code.iter().all(u8::is_ascii_digit)
                && !bytes
                    .get(start + 3)
                    .is_some_and(|c| c.is_ascii_alphanumeric())
        })
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

    fn is_transient_for_test(stderr: &str) -> bool {
        is_transient(stderr, "alpine:3.20")
    }

    const THROTTLED: &str = "Error response from daemon: unexpected status from HEAD request to https://public.ecr.aws/v2/docker/library/alpine/manifests/3.20: 429 Too Many Requests";

    #[tokio::test]
    async fn a_successful_pull_needs_no_cache_check() {
        let cli = FakeCli::new(0, "", false);
        assert_eq!(cli.pull().await, Ok(PulledImage::Pulled));
        assert_eq!(cli.calls(), ["pull alpine:3.20"]);
    }

    #[tokio::test]
    async fn a_throttled_pull_uses_the_cached_image() {
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
        assert_eq!(
            cli.calls(),
            ["pull alpine:3.20"],
            "a refused pull is neither retried nor checked against the cache"
        );
    }

    #[tokio::test]
    async fn a_refused_pull_fails_even_with_a_stale_cached_copy() {
        // The image was deleted from the registry, or a policy now denies the
        // pull. A copy cached by an earlier launch must not be used.
        for refused in [
            "Error response from daemon: manifest for alpine:3.20 not found: manifest unknown",
            "Error response from daemon: pull access denied for alpine, repository does not exist or may require authorization: denied",
        ] {
            let cli = FakeCli::new(u32::MAX, refused, true);
            assert_eq!(cli.pull().await, Err(refused.to_string()));
            assert_eq!(cli.calls(), ["pull alpine:3.20"]);
        }
    }

    #[tokio::test]
    async fn a_registry_server_error_uses_the_cached_image() {
        let unavailable =
            "Error response from daemon: received unexpected HTTP status: 503 Service Unavailable";
        let cli = FakeCli::new(u32::MAX, unavailable, true);
        assert_eq!(
            cli.pull().await,
            Ok(PulledImage::Cached {
                pull_error: unavailable.to_string()
            })
        );
    }

    #[tokio::test]
    async fn a_refused_pull_of_a_repository_named_like_a_marker_is_still_refused() {
        // The message quotes the image name. A repository spelled like a
        // throttling code must not make a refused pull look transient.
        let refused = "Error response from daemon: manifest for toomanyrequests:latest not found: manifest unknown: manifest unknown";
        let cli = FakeCli::new(u32::MAX, refused, true);
        assert_eq!(cli.pull().await, Err(refused.to_string()));
        assert_eq!(cli.calls(), ["pull alpine:3.20"]);
    }

    #[test]
    fn transient_detection_separates_retryable_from_refused() {
        assert!(is_transient_for_test(THROTTLED));
        assert!(is_transient_for_test(
            "toomanyrequests: You have reached your pull rate limit."
        ));
        assert!(is_transient_for_test("Error: Rate exceeded"));
        assert!(is_transient_for_test(
            "received unexpected HTTP status: 502 Bad Gateway"
        ));
        assert!(is_transient_for_test(
            "Get \"https://public.ecr.aws/v2/\": net/http: TLS handshake timeout"
        ));
        assert!(is_transient_for_test(
            "read tcp 10.0.0.2:4431->1.2.3.4:443: read: connection reset by peer"
        ));
        assert!(!is_transient_for_test("manifest unknown"));
        assert!(!is_transient_for_test("pull access denied for foo"));
        assert!(!is_transient_for_test(
            "unauthorized: authentication required"
        ));
        assert!(!is_transient_for_test(
            "pull access denied for toomanyrequests, repository does not exist or may require authorization"
        ));
    }

    #[test]
    fn any_5xx_status_is_transient_whatever_its_reason_phrase() {
        for msg in [
            "received unexpected HTTP status: 500 Internal Server Error",
            "unexpected status from GET request to https://r.example/v2/: 507 Insufficient Storage",
            "unexpected status code 520",
            "error pulling image: status: 599",
            "unexpected status from HEAD request to https://r.example/v2/a/manifests/1: 503",
        ] {
            assert!(is_transient_for_test(msg), "{msg}");
        }
        for msg in [
            // A registry port or a 4xx is not a server error.
            "Get \"http://127.0.0.1:5000/v2/\": dial tcp 127.0.0.1:5000: connect: connection refused",
            "unexpected status code 400 Bad Request",
            "status: 5001",
        ] {
            assert!(!is_transient_for_test(msg), "{msg}");
        }
    }

    #[test]
    fn a_throttled_pull_of_a_repository_named_like_a_refusal_is_still_transient() {
        let reference = "public.ecr.aws/acme/access-denied-page:1";
        for msg in [
            "Error response from daemon: unexpected status from HEAD request to https://public.ecr.aws/v2/acme/access-denied-page/manifests/1: 429 Too Many Requests",
            "Error response from daemon: toomanyrequests: Rate exceeded for public.ecr.aws/acme/access-denied-page:1",
        ] {
            assert!(is_transient(msg, reference), "{msg}");
        }
        // Its genuine refusals are still refusals.
        assert!(!is_transient(
            "Error response from daemon: manifest for public.ecr.aws/acme/access-denied-page:1 not found: manifest unknown",
            reference
        ));
    }

    #[test]
    fn only_whole_names_are_removed() {
        // A one-letter repository must not cut the `d` out of `denied`.
        assert!(!is_transient(
            "Error response from daemon: pull access denied for d, repository does not exist",
            "d"
        ));
        assert_eq!(
            without_image_name(
                "pull access denied for docker.io/library/alpine",
                "alpine:3.20"
            ),
            "pull access denied for docker.io/library/ "
        );
        assert_eq!(
            without_image_name(
                "get https://127.0.0.1:5000/v2/team/app/manifests/v1",
                "127.0.0.1:5000/team/app:v1"
            ),
            "get https://127.0.0.1:5000/v2/ /manifests/v1"
        );
    }
}
