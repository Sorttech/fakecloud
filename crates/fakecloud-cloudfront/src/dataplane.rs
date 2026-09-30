// crates/fakecloud-cloudfront/src/dataplane.rs
//! In-process CloudFront data plane.
//!
//! Distributions are served on fakecloud's **main `--addr` listener**, routed by
//! the request `Host` header, rather than on a per-distribution ephemeral port.
//! [`CloudFrontDataPlane::serve`] is installed as an outer middleware on the main
//! axum router: it matches the `Host` header against every enabled distribution's
//! `DomainName` (`<id>.cloudfront.net`) or one of its alternate domain names
//! (`Aliases`/CNAMEs). A match is served as viewer traffic; anything else (the AWS
//! API, `/_fakecloud/*`, health) is handed straight back for normal dispatch.
//!
//! This is how real CloudFront works -- a distribution is reached by its domain,
//! not a port -- and it means a distribution is reachable from outside a container
//! whenever the main port is published (`-p`), with no second listener to expose.
//! Clients discover which distributions are served, and the domain to send as
//! `Host`, via `/_fakecloud/cloudfront/distributions`.
//!
//! Once a request is matched to a distribution, [`serve`](CloudFrontDataPlane::serve)
//! selects a cache behavior by path pattern, resolves its origin, reverse-proxies
//! to it, and applies CustomErrorResponses (e.g. the SPA `404 -> /index.html`
//! served as `200`). There is no global edge network -- this is a single local
//! origin-serving node, matching the ALB/API Gateway precedent. Deferred (not
//! implemented): in-path CloudFront Functions / Lambda@Edge and TTL caching /
//! invalidation.
//!
//! S3 origins live in this same process, so they are fetched in-process through
//! the rest of the server's middleware stack rather than over a socket. That is
//! also how private S3 origins work: an origin with an origin access control
//! (`OriginAccessControlId`, honoring its `SigningBehavior`) is fetched as the
//! `cloudfront.amazonaws.com` service principal with `aws:SourceArn` = the
//! distribution ARN and `aws:SourceAccount` = its owner, and one with a legacy
//! origin access identity (`S3OriginConfig.OriginAccessIdentity`) as that OAI,
//! so under IAM enforcement the bucket policy decides exactly as in AWS. The
//! identity rides a request extension ([`InternalCaller`]) that no client can
//! set. Origins with neither are fetched anonymously.

use std::time::Duration;

use axum::body::Body;
use axum::extract::{ConnectInfo, Request};
use axum::middleware::Next;
use axum::response::Response;
use bytes::Bytes;
use fakecloud_core::auth::InternalCaller;
use http::{header, HeaderMap, Method, StatusCode};
use tracing::{trace, warn};

use crate::model::DistributionConfig;
use crate::state::{AccountState, CloudFrontAccounts, SharedCloudFrontState, StoredDistribution};

/// The service principal CloudFront signs origin requests as under an origin
/// access control.
const CLOUDFRONT_SERVICE_PRINCIPAL: &str = "cloudfront.amazonaws.com";

const ENV_DISABLE: &str = "FAKECLOUD_CLOUDFRONT_DISABLE_DATAPLANE";

/// Whether the data plane should serve viewer traffic. Disabled by setting
/// `FAKECLOUD_CLOUDFRONT_DISABLE_DATAPLANE` to a truthy value (mirrors the ELBv2
/// flag), for environments that only exercise the control plane. Also drives the
/// `served` flag surfaced via `/_fakecloud/cloudfront/distributions`.
pub fn dataplane_enabled() -> bool {
    !matches!(
        std::env::var(ENV_DISABLE).as_deref(),
        Ok("1") | Ok("true") | Ok("TRUE") | Ok("yes") | Ok("YES")
    )
}

/// The CloudFront data plane: serves enabled distributions on the main listener,
/// routed by `Host`. Constructed once at server startup and installed as an outer
/// middleware; see [`CloudFrontDataPlane::serve`].
pub struct CloudFrontDataPlane {
    state: SharedCloudFrontState,
    /// HTTP client used to fetch from remote (custom) origins (reverse-proxy).
    upstream: reqwest::Client,
    /// `host:port` of fakecloud's own server. S3 origins are served by this
    /// same process; their URLs are built against it (with the bucket domain
    /// preserved in the `Host` header) and dispatched in-process.
    s3_endpoint: String,
    /// Cached `dataplane_enabled()` at construction: when false, `serve` never
    /// intercepts and every request falls through to normal AWS dispatch.
    enabled: bool,
}

impl CloudFrontDataPlane {
    /// Build the data plane. `server_port` is fakecloud's own listen port, used to
    /// reach S3-website origins served by this same process. Returns an `Arc` for
    /// sharing into the axum middleware layer. Cheap and infallible; if the tuned
    /// reqwest client fails to build (should not happen), the plane declines to
    /// serve (`enabled = false`) so requests still dispatch normally rather than
    /// being proxied through a degraded client.
    pub fn new(state: SharedCloudFrontState, server_port: u16) -> std::sync::Arc<Self> {
        let mut enabled = dataplane_enabled();
        let upstream = reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap_or_else(|e| {
                warn!(
                    "CloudFront data plane: failed to build reqwest client: {e}; serving disabled"
                );
                // Decline to serve: a default client lacks the invalid-cert /
                // no-redirect / timeout behavior the data plane relies on, so
                // proxying through it would silently misbehave.
                enabled = false;
                reqwest::Client::new()
            });
        std::sync::Arc::new(Self {
            state,
            upstream,
            s3_endpoint: format!("127.0.0.1:{server_port}"),
            enabled,
        })
    }

    /// Serve a request iff its `Host` matches an enabled distribution.
    ///
    /// `next` is the rest of the server's middleware stack (AWS dispatch). A
    /// request whose `Host` matches no distribution (or any request, when the
    /// plane is disabled) is handed to it untouched. A matched request is viewer
    /// traffic: it is proxied to the resolved origin, and `next` is also how an
    /// S3 origin -- served by this same process -- is fetched.
    ///
    /// The `Host` check happens on the request headers before the body is touched,
    /// so pass-through traffic (all AWS API calls, `/_fakecloud/*`) is never
    /// buffered.
    pub async fn serve(&self, req: Request<Body>, next: Next) -> Response {
        if !self.enabled {
            return next.run(req).await;
        }
        // Prefer the `Host` header (HTTP/1.1); fall back to the URI authority so
        // HTTP/2 viewer requests (which carry the domain in `:authority` and may
        // omit `Host`) still route to a distribution.
        let host = req
            .headers()
            .get(header::HOST)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string())
            .or_else(|| req.uri().host().map(|h| h.to_string()));
        let Some(host) = host else {
            return next.run(req).await;
        };

        // Resolve the route under the read lock (owned snapshot so the guard drops
        // at the end of the block). The outer `Option` distinguishes "no
        // distribution serves this Host" (fall through) from "a distribution
        // matched but has no usable origin" (serve a 502 -- it IS our traffic).
        let matched: Option<Option<RouteResolution>> = {
            let accs = self.state.read();
            find_distribution_by_host(&accs, &host).map(|(account_id, d)| {
                let ctx = RouteContext {
                    distribution_arn: &d.arn,
                    account_id,
                    account: accs.get(account_id),
                };
                resolve_route(&d.config, req.uri().path(), &self.s3_endpoint, &ctx)
            })
        };
        let Some(route_opt) = matched else {
            return next.run(req).await;
        };

        // From here the request belongs to CloudFront: consume it and proxy.
        let (parts, body) = req.into_parts();
        // Apply the SAME buffered-body cap as direct (non-viewer) traffic
        // (`FAKECLOUD_MAX_REQUEST_BODY_BYTES`, default 1 GiB) so a request isn't
        // rejected merely because it went through a distribution.
        let max_body = fakecloud_core::dispatch::max_request_body_bytes();
        let body_bytes = match axum::body::to_bytes(body, max_body).await {
            Ok(b) => b,
            Err(_) => {
                return canned(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "viewer request body too large",
                )
            }
        };
        let Some(route) = route_opt else {
            return canned(
                StatusCode::BAD_GATEWAY,
                "distribution has no matching origin",
            );
        };

        // A root request is fetched as the distribution's DefaultRootObject; the
        // query string is preserved, as CloudFront does.
        let path_and_query = match &route.root_object {
            Some(object) => match parts.uri.query() {
                Some(q) => format!("{object}?{q}"),
                None => object.clone(),
            },
            None => parts
                .uri
                .path_and_query()
                .map(|p| p.as_str())
                .unwrap_or("/")
                .to_string(),
        };
        let url = format!("{}{path_and_query}", route.upstream.url_base);
        trace!(%host, path = %parts.uri.path(), origin = %route.upstream.host_header, "CloudFront data plane: proxying");
        let resp = self
            .fetch_origin(
                &next,
                &route.upstream,
                &parts.method,
                &url,
                &parts.headers,
                &body_bytes,
            )
            .await;

        // CustomErrorResponses: if the origin status matches a configured rule with
        // a response page path, serve that page from the DEFAULT origin and return
        // it with the rule's response code (the SPA deep-link fallback, e.g.
        // 404 -> /index.html returned as 200).
        if let Some(rule) = match_error_rule(&route.error_rules, resp.status().as_u16()) {
            let origin_status = resp.status();
            let url = format!("{}{}", route.default_upstream.url_base, rule.page_path);
            let err_resp = self
                .fetch_origin(
                    &next,
                    &route.default_upstream,
                    &Method::GET,
                    &url,
                    &HeaderMap::new(),
                    &Bytes::new(),
                )
                .await;
            // Only interpose the custom error page when the fallback fetch itself
            // succeeded. If fetching the page failed (e.g. the default origin is
            // down, or the page path 404s), returning it under the rule's success
            // ResponseCode would mask an error body with a 200; keep the ORIGINAL
            // origin response instead.
            if err_resp.status().is_success() {
                let mut err_resp = err_resp;
                // Status = the rule's ResponseCode if set, else the ORIGINAL origin
                // error status (AWS: an omitted ResponseCode keeps the origin's code).
                let final_status = rule
                    .response_code
                    .and_then(|c| StatusCode::from_u16(c).ok())
                    .unwrap_or(origin_status);
                *err_resp.status_mut() = final_status;
                return err_resp;
            }
            return resp;
        }
        resp
    }

    /// Fetch `url` from the resolved origin: in-process through `next` for an
    /// S3 origin this process serves, over HTTP otherwise.
    async fn fetch_origin(
        &self,
        next: &Next,
        target: &UpstreamTarget,
        method: &Method,
        url: &str,
        req_headers: &HeaderMap,
        body: &Bytes,
    ) -> Response {
        if target.local {
            return fetch_local(next, target, method, url, req_headers, body).await;
        }
        let host_header = target.host_header.as_str();
        let mut rb = self.upstream.request(reqwest_method(method), url);
        for (k, v) in req_headers.iter() {
            let n = k.as_str();
            if is_hop_by_hop(n) || n.eq_ignore_ascii_case("host") {
                continue;
            }
            rb = rb.header(k.as_str(), v.as_bytes());
        }
        rb = rb.header("host", host_header);
        if !body.is_empty() {
            rb = rb.body(body.to_vec());
        }
        match rb.send().await {
            Ok(up) => {
                let status = up.status();
                let headers = up.headers().clone();
                let bytes = up.bytes().await.unwrap_or_default();
                let mut builder = Response::builder().status(status);
                for (k, v) in headers.iter() {
                    if !is_hop_by_hop(k.as_str()) {
                        builder = builder.header(k, v);
                    }
                }
                builder
                    .body(Body::from(bytes))
                    .unwrap_or_else(|_| canned(StatusCode::BAD_GATEWAY, "invalid origin response"))
            }
            Err(e) => canned(StatusCode::BAD_GATEWAY, &format!("origin error: {e}")),
        }
    }
}

/// Find the enabled distribution whose `DomainName` (`<id>.cloudfront.net`) or one
/// of its alternate domain names (`Aliases`/CNAMEs) matches `host`, paired with
/// its owning account. The port is stripped and matching is case-insensitive.
/// Alternate domain names are exact in CloudFront (not wildcards), so this is an
/// exact host compare, mirroring the route53 CloudFront resolver.
pub(crate) fn find_distribution_by_host<'a>(
    accs: &'a CloudFrontAccounts,
    host: &str,
) -> Option<(&'a str, &'a StoredDistribution)> {
    let host = host.split(':').next().unwrap_or(host).trim();
    if host.is_empty() {
        return None;
    }
    accs.all_distributions()
        .map(|(account_id, d)| (account_id.as_str(), d))
        .filter(|(_, d)| d.config.enabled)
        .find(|(_, d)| {
            d.domain_name.eq_ignore_ascii_case(host)
                || d.config
                    .aliases
                    .as_ref()
                    .and_then(|a| a.items.as_ref())
                    .is_some_and(|it| it.cname.iter().any(|c| c.eq_ignore_ascii_case(host)))
        })
}

/// Owned per-request routing snapshot (taken under the state read lock).
struct RouteResolution {
    /// Resolved upstream for the matched cache behavior.
    upstream: UpstreamTarget,
    /// Resolved upstream for the default cache behavior (where
    /// CustomErrorResponse pages are fetched from).
    default_upstream: UpstreamTarget,
    /// CustomErrorResponses that have a response page path.
    error_rules: Vec<ErrorRule>,
    /// `DefaultRootObject` as a percent-encoded path (`/index.html`), set only when this
    /// request is for the distribution root and the distribution configures one.
    root_object: Option<String>,
}

/// A resolved origin address: the scheme+authority to connect to and the `Host`
/// header to send.
#[derive(Clone)]
struct UpstreamTarget {
    /// `scheme://authority` plus the origin's `OriginPath` (no trailing slash);
    /// the request path is appended.
    url_base: String,
    /// `Host` header sent upstream (the origin domain name).
    host_header: String,
    /// An S3 origin this process serves: fetched in-process through the rest
    /// of the middleware stack rather than over HTTP.
    local: bool,
    /// Who the origin request is made as. Only local S3 origins are signed.
    auth: OriginAuth,
}

/// Who CloudFront makes an origin request as.
#[derive(Clone, Debug, PartialEq, Eq)]
enum OriginAuth {
    /// Unsigned: an origin with neither an origin access control nor an
    /// origin access identity, or an OAC whose `SigningBehavior` is `never`.
    Anonymous,
    /// Always signed as the caller, replacing any viewer `Authorization`
    /// header: OAC `SigningBehavior: always`, and a legacy OAI.
    Always(InternalCaller),
    /// OAC `SigningBehavior: no-override`: signed unless the viewer request
    /// carries its own `Authorization` header, which is then forwarded as is.
    NoOverride(InternalCaller),
}

impl OriginAuth {
    /// The identity to make the origin request as, given the viewer's headers
    /// (`None` = anonymous or the viewer's own credentials).
    fn caller_for(&self, viewer_headers: &HeaderMap) -> Option<&InternalCaller> {
        match self {
            OriginAuth::Anonymous => None,
            OriginAuth::Always(caller) => Some(caller),
            OriginAuth::NoOverride(caller) => {
                (!viewer_headers.contains_key(header::AUTHORIZATION)).then_some(caller)
            }
        }
    }
}

/// What routing needs to know about the matched distribution beyond its config:
/// its identity (for signed origin requests) and its account's resources (the
/// origin access controls and identities its origins reference).
struct RouteContext<'a> {
    distribution_arn: &'a str,
    account_id: &'a str,
    account: Option<&'a AccountState>,
}

#[derive(Clone)]
struct ErrorRule {
    error_code: u16,
    page_path: String,
    response_code: Option<u16>,
}

/// Resolve the matched origin, the default origin, and the custom-error rules
/// for a request path.
fn resolve_route(
    cfg: &DistributionConfig,
    path: &str,
    s3_endpoint: &str,
    ctx: &RouteContext<'_>,
) -> Option<RouteResolution> {
    let items = cfg.origins.items.as_ref()?;
    let target = select_target_origin(cfg, path);
    let upstream = items
        .origin
        .iter()
        .find(|o| o.id == target)
        .map(|o| origin_target(o, s3_endpoint, ctx))?;
    let default_target = cfg.default_cache_behavior.target_origin_id.as_str();
    let default_upstream = items
        .origin
        .iter()
        .find(|o| o.id == default_target)
        .map(|o| origin_target(o, s3_endpoint, ctx))
        .unwrap_or_else(|| upstream.clone());
    let error_rules = cfg
        .custom_error_responses
        .as_ref()
        .and_then(|c| c.items.as_ref())
        .map(|it| {
            it.custom_error_response
                .iter()
                .filter_map(|r| {
                    r.response_page_path.as_ref().map(|p| ErrorRule {
                        error_code: r.error_code as u16,
                        page_path: p.clone(),
                        response_code: r.response_code.as_ref().and_then(|s| s.parse().ok()),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let root_object = root_object_path(cfg, path);
    Some(RouteResolution {
        upstream,
        default_upstream,
        error_rules,
        root_object,
    })
}

/// The origin path to fetch in place of the viewer's `path` when it names the
/// distribution root and a `DefaultRootObject` is configured.
///
/// AWS applies the default root object to the root only: a subdirectory request
/// (`/about/`) is never rewritten to `<dir>/<object>`, and `///` is not treated as
/// the root either (the origin makes that comparison). The cache behavior is still
/// selected on the original viewer path: the substitution happens after behavior
/// selection, which is why a viewer-request function on the matched behavior still
/// sees `/` as the URI.
///
/// An empty value is how the API clears the setting, so it means "none". Any
/// other value is appended verbatim after `/`: AWS does not strip a leading
/// slash, so `/index.html` is requested as `//index.html` (which is why AWS warns
/// that such a value "can lead to a 403 Access Denied error").
fn root_object_path(cfg: &DistributionConfig, path: &str) -> Option<String> {
    // `""` only arises for a request-target with no path at all, which HTTP
    // treats as `/`.
    if !matches!(path, "" | "/") {
        return None;
    }
    cfg.default_root_object
        .as_deref()
        .filter(|o| !o.is_empty())
        .map(|o| format!("/{}", encode_path(o)))
}

/// Resolve `origin` to its upstream and apply its `OriginPath`.
///
/// CloudFront prefixes every request it sends to an origin (viewer requests,
/// the default root object, custom error pages) with that origin's
/// `OriginPath`: OriginPath `/prod` + viewer `/img/a.png` fetches
/// `/prod/img/a.png`. It is folded into `url_base`, so every path appended to
/// it is prefixed. AWS requires the value to start with `/` and not end with
/// one; since only the leading `/` is structurally required to keep the value
/// out of the URL authority, it is added if missing and trailing slashes are
/// dropped so `/prod/` cannot produce `/prod//img`.
///
/// A local S3 origin also gets the identity CloudFront makes its requests as
/// ([`origin_auth`]).
fn origin_target(
    origin: &crate::model::Origin,
    s3_endpoint: &str,
    ctx: &RouteContext<'_>,
) -> UpstreamTarget {
    let mut target = upstream_for(origin, s3_endpoint);
    if target.local {
        target.auth = origin_auth(origin, ctx);
    }
    let prefix = origin
        .origin_path
        .as_deref()
        .map(|p| p.trim_end_matches('/'))
        .unwrap_or_default();
    if !prefix.is_empty() {
        if !prefix.starts_with('/') {
            target.url_base.push('/');
        }
        target.url_base.push_str(&encode_path(prefix));
    }
    target
}

/// Percent-encode a configured path (a `DefaultRootObject` or `OriginPath`) for
/// use as a URL path. The value names an object literally, so every byte that
/// is not a valid path-segment character (RFC 3986 `pchar`) is escaped --
/// including `#`, `?`, space, `%` and non-ASCII -- while `/` is kept so folder
/// values like `app/index.html` still address that folder.
fn encode_path(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for b in raw.bytes() {
        let keep = b.is_ascii_alphanumeric()
            || matches!(
                b,
                b'/' | b'-'
                    | b'.'
                    | b'_'
                    | b'~'
                    | b'!'
                    | b'$'
                    | b'&'
                    | b'\''
                    | b'('
                    | b')'
                    | b'*'
                    | b'+'
                    | b','
                    | b';'
                    | b'='
                    | b':'
                    | b'@'
            );
        if keep {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// First custom-error rule whose error code matches the origin status.
fn match_error_rule(rules: &[ErrorRule], status: u16) -> Option<ErrorRule> {
    rules.iter().find(|r| r.error_code == status).cloned()
}

fn select_target_origin<'a>(cfg: &'a DistributionConfig, path: &str) -> &'a str {
    if let Some(cbs) = &cfg.cache_behaviors {
        if let Some(items) = &cbs.items {
            for cb in &items.cache_behavior {
                if path_pattern_matches(&cb.path_pattern, path) {
                    return &cb.target_origin_id;
                }
            }
        }
    }
    &cfg.default_cache_behavior.target_origin_id
}

/// An S3 origin this process serves: a virtual-hosted S3 endpoint naming a
/// bucket, in any of the forms AWS publishes. That covers the REST endpoints
/// an `S3OriginConfig` origin carries (`<bucket>.s3.<region>.amazonaws.com`,
/// CDK's `S3BucketOrigin` emitting the bucket's `RegionalDomainName`; the
/// legacy `<bucket>.s3.amazonaws.com`; the dash-separated
/// `<bucket>.s3-<region>.amazonaws.com`; dualstack and FIPS) and the
/// static-website endpoints (`<bucket>.s3-website-<region>.amazonaws.com`,
/// `<bucket>.s3-website.<region>.amazonaws.com`), under every partition's DNS
/// suffix.
///
/// Delegates to the shared `Host` parser, so a domain is recognized exactly
/// when the S3 front door can resolve its bucket from `Host` (dotted bucket
/// names, case-insensitivity and the LocalStack hostname convention included).
/// A bucket is required: a path-style `s3.<region>.amazonaws.com` has nothing
/// to serve, and a look-alike such as `my.s3-website.example.com` is not an
/// AWS hostname at all, so neither is rerouted.
fn is_s3_origin(domain: &str) -> bool {
    fakecloud_core::protocol::parse_routing_host(domain)
        .is_some_and(|h| h.service == "s3" && h.bucket.is_some())
}

/// An S3 static-website endpoint (`<bucket>.s3-website-<region>...` or
/// `<bucket>.s3-website.<region>...`). CloudFront treats one as a custom
/// origin: it serves public content only, and origin access controls and
/// identities never apply to it.
fn is_s3_website_origin(domain: &str) -> bool {
    let Some(bucket) = fakecloud_core::protocol::parse_routing_host(domain).and_then(|h| h.bucket)
    else {
        return false;
    };
    let domain = domain.to_ascii_lowercase();
    domain
        .strip_prefix(&format!("{}.", bucket.to_ascii_lowercase()))
        .is_some_and(|endpoint| endpoint.starts_with("s3-website"))
}

/// The identity CloudFront fetches a local S3 origin as.
///
/// - `OriginAccessControlId` names an origin access control of the
///   distribution's account with origin type `s3`: sign as the
///   `cloudfront.amazonaws.com` service principal with `aws:SourceArn` = the
///   distribution ARN and `aws:SourceAccount` = its account, per the OAC's
///   `SigningBehavior` (`always`, `no-override`; `never` is unsigned).
/// - Otherwise `S3OriginConfig.OriginAccessIdentity`
///   (`origin-access-identity/cloudfront/<id>`) names an origin access
///   identity of the account: sign as that OAI
///   (`arn:<partition>:iam::cloudfront:user/CloudFront Origin Access Identity <id>`,
///   canonical user = its `S3CanonicalUserId`).
/// - Otherwise, or for an S3 website endpoint, or when the referenced OAC /
///   OAI does not exist, anonymous: an unknown identity never grants.
///
/// An origin carrying both uses the OAC, as CloudFront does.
fn origin_auth(origin: &crate::model::Origin, ctx: &RouteContext<'_>) -> OriginAuth {
    if is_s3_website_origin(&origin.domain_name) {
        return OriginAuth::Anonymous;
    }
    if let Some(oac_id) = origin
        .origin_access_control_id
        .as_deref()
        .filter(|id| !id.is_empty())
    {
        let Some(oac) = ctx
            .account
            .and_then(|a| a.origin_access_controls.get(oac_id))
            .filter(|oac| {
                oac.config
                    .origin_access_control_origin_type
                    .eq_ignore_ascii_case("s3")
            })
        else {
            return OriginAuth::Anonymous;
        };
        let caller = InternalCaller::Service {
            service: CLOUDFRONT_SERVICE_PRINCIPAL.to_string(),
            source_arn: ctx.distribution_arn.to_string(),
            source_account: ctx.account_id.to_string(),
        };
        return match oac.config.signing_behavior.to_ascii_lowercase().as_str() {
            "always" => OriginAuth::Always(caller),
            "no-override" => OriginAuth::NoOverride(caller),
            _ => OriginAuth::Anonymous,
        };
    }
    let oai_id = origin
        .s3_origin_config
        .as_ref()
        .map(|c| c.origin_access_identity.trim())
        .and_then(|oai| oai.strip_prefix("origin-access-identity/cloudfront/"))
        .filter(|id| !id.is_empty());
    let Some(oai) =
        oai_id.and_then(|id| ctx.account.and_then(|a| a.origin_access_identities.get(id)))
    else {
        return OriginAuth::Anonymous;
    };
    let partition = ctx
        .distribution_arn
        .split(':')
        .nth(1)
        .filter(|p| !p.is_empty())
        .unwrap_or("aws");
    OriginAuth::Always(InternalCaller::ServiceOwned {
        arn: format!(
            "arn:{partition}:iam::cloudfront:user/CloudFront Origin Access Identity {}",
            oai.id
        ),
        canonical_user_id: Some(oai.s3_canonical_user_id.clone()),
        acting_account: ctx.account_id.to_string(),
    })
}

/// The origin-form request target (`/path?query`) of an assembled origin URL,
/// percent-encoded the way an HTTP client sends it: `OriginPath` and a custom
/// error `ResponsePagePath` are joined as configured, so they may hold bytes
/// (a space, say) a request line cannot carry raw. `None` only for a URL no
/// client could send.
fn local_path_and_query(url: &str) -> Option<String> {
    let parsed = reqwest::Url::parse(url).ok()?;
    let mut target = parsed.path().to_string();
    if let Some(query) = parsed.query() {
        target.push('?');
        target.push_str(query);
    }
    Some(target)
}

/// Fetch `url` from an S3 origin this process serves, in-process through the
/// rest of the middleware stack, as the identity `target.auth` selects for this
/// viewer request. A signed request drops the viewer's `Authorization` header
/// (CloudFront replaces it with its own signature) and carries the identity as
/// an [`InternalCaller`] extension, which dispatch authorizes against the
/// bucket policy. The source address is loopback, as the connection would be.
async fn fetch_local(
    next: &Next,
    target: &UpstreamTarget,
    method: &Method,
    url: &str,
    req_headers: &HeaderMap,
    body: &Bytes,
) -> Response {
    let Some(path_and_query) = local_path_and_query(url) else {
        return canned(
            StatusCode::BAD_GATEWAY,
            &format!("invalid origin URL: {url}"),
        );
    };
    let caller = target.auth.caller_for(req_headers).cloned();
    let mut builder = Request::builder()
        .method(method.clone())
        .uri(path_and_query);
    for (k, v) in req_headers.iter() {
        let n = k.as_str();
        if is_hop_by_hop(n)
            || n.eq_ignore_ascii_case("host")
            || (caller.is_some() && n.eq_ignore_ascii_case("authorization"))
        {
            continue;
        }
        builder = builder.header(k, v);
    }
    builder = builder.header(header::HOST, target.host_header.as_str());
    let mut req = match builder.body(Body::from(body.clone())) {
        Ok(req) => req,
        Err(e) => return canned(StatusCode::BAD_GATEWAY, &format!("origin error: {e}")),
    };
    req.extensions_mut()
        .insert(ConnectInfo(std::net::SocketAddr::from(([127, 0, 0, 1], 0))));
    if let Some(caller) = caller {
        req.extensions_mut().insert(caller);
    }
    next.clone().run(req).await
}

/// Resolve an [`crate::model::Origin`] to the upstream to connect to.
///
/// - S3 origins (REST or static-website endpoints naming a bucket) are served by
///   this same fakecloud process, so connect to its own port while preserving
///   the bucket domain in `Host`, where the S3 front door reads the bucket from.
///   This holds even when the origin is declared with a `CustomOriginConfig`
///   (website endpoints always are, and a REST endpoint may be): the bucket
///   lives here, and the real hostname would reach real AWS.
/// - Custom origins honor `CustomOriginConfig`: an `https-only` protocol policy
///   is fetched over HTTPS (else HTTP), and the configured `HTTPPort`/`HTTPSPort`
///   is appended UNLESS the `domain_name` already carries an explicit `:port`
///   (as local test origins do) or the port is the scheme default.
/// - Bare origins (no config) are reached over HTTP at their domain verbatim.
fn upstream_for(origin: &crate::model::Origin, s3_endpoint: &str) -> UpstreamTarget {
    let domain = &origin.domain_name;
    if is_s3_origin(domain) {
        return UpstreamTarget {
            url_base: format!("http://{s3_endpoint}"),
            host_header: domain.clone(),
            local: true,
            auth: OriginAuth::Anonymous,
        };
    }
    if let Some(cfg) = &origin.custom_origin_config {
        let https = cfg
            .origin_protocol_policy
            .eq_ignore_ascii_case("https-only");
        let (scheme, port) = if https {
            ("https", cfg.https_port)
        } else {
            ("http", cfg.http_port)
        };
        // A domain that already encodes a port (host:port, as local origins do)
        // wins over the config port; otherwise append a non-default port.
        let has_explicit_port = domain.rsplit(':').next().is_some_and(|s| {
            !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) && domain.contains(':')
        });
        let default_port = (scheme == "http" && port == 80) || (scheme == "https" && port == 443);
        let authority = if has_explicit_port || port <= 0 || default_port {
            domain.clone()
        } else {
            format!("{domain}:{port}")
        };
        return UpstreamTarget {
            url_base: format!("{scheme}://{authority}"),
            host_header: domain.clone(),
            local: false,
            auth: OriginAuth::Anonymous,
        };
    }
    UpstreamTarget {
        url_base: format!("http://{domain}"),
        host_header: domain.clone(),
        local: false,
        auth: OriginAuth::Anonymous,
    }
}

/// Match a CloudFront cache-behavior path pattern (`*` = any sequence, `?` = one
/// character) against a request path. AWS path patterns are relative (no leading
/// slash, e.g. `api/*`); normalize both sides so a canonical `api/*` and a
/// slash-prefixed `/api/*` both match a request path like `/api/orders`.
fn path_pattern_matches(pattern: &str, path: &str) -> bool {
    let pat = pattern.trim_start_matches('/');
    let p = path.trim_start_matches('/');
    glob_match(pat.as_bytes(), p.as_bytes())
}

fn glob_match(pat: &[u8], text: &[u8]) -> bool {
    // Iterative glob with backtracking on `*`.
    let (mut p, mut t) = (0usize, 0usize);
    let (mut star, mut mark) = (None, 0usize);
    while t < text.len() {
        if p < pat.len() && (pat[p] == b'?' || pat[p] == text[t]) {
            p += 1;
            t += 1;
        } else if p < pat.len() && pat[p] == b'*' {
            star = Some(p);
            mark = t;
            p += 1;
        } else if let Some(sp) = star {
            p = sp + 1;
            mark += 1;
            t = mark;
        } else {
            return false;
        }
    }
    while p < pat.len() && pat[p] == b'*' {
        p += 1;
    }
    p == pat.len()
}

fn canned(status: StatusCode, msg: &str) -> Response {
    Response::builder()
        .status(status)
        .body(Body::from(msg.to_string()))
        .expect("canned response builds")
}

fn reqwest_method(m: &Method) -> reqwest::Method {
    reqwest::Method::from_bytes(m.as_str().as_bytes()).unwrap_or(reqwest::Method::GET)
}

const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

fn is_hop_by_hop(name: &str) -> bool {
    HOP_BY_HOP.iter().any(|&h| h.eq_ignore_ascii_case(name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        AliasItems, Aliases, CustomOriginConfig, DefaultCacheBehavior, Origin, OriginItems, Origins,
    };
    use crate::state::StoredDistribution;
    use chrono::Utc;

    fn origin(domain: &str, custom: Option<CustomOriginConfig>) -> Origin {
        Origin {
            id: "o".into(),
            domain_name: domain.into(),
            custom_origin_config: custom,
            ..Default::default()
        }
    }

    fn custom(policy: &str, http_port: i32, https_port: i32) -> CustomOriginConfig {
        CustomOriginConfig {
            http_port,
            https_port,
            origin_protocol_policy: policy.into(),
            ..Default::default()
        }
    }

    fn dist(id: &str, enabled: bool, aliases: &[&str]) -> StoredDistribution {
        let mut config = DistributionConfig {
            enabled,
            ..Default::default()
        };
        if !aliases.is_empty() {
            config.aliases = Some(Aliases {
                quantity: aliases.len() as i32,
                items: Some(AliasItems {
                    cname: aliases.iter().map(|s| s.to_string()).collect(),
                }),
            });
        }
        StoredDistribution {
            id: id.to_string(),
            arn: format!("arn:aws:cloudfront::123456789012:distribution/{id}"),
            status: "Deployed".into(),
            last_modified_time: Utc::now(),
            domain_name: format!("{}.cloudfront.net", id.to_lowercase()),
            in_progress_invalidation_batches: 0,
            etag: "E1".into(),
            config,
        }
    }

    /// A route context for a distribution whose account holds no origin
    /// access controls or identities.
    fn no_ctx() -> RouteContext<'static> {
        RouteContext {
            distribution_arn: "arn:aws:cloudfront::123456789012:distribution/E1ABC",
            account_id: "123456789012",
            account: None,
        }
    }

    fn accounts_with(dists: Vec<StoredDistribution>) -> CloudFrontAccounts {
        let mut accs = CloudFrontAccounts::new();
        let acct = accs.entry("123456789012");
        for d in dists {
            acct.distributions.insert(d.id.clone(), d);
        }
        accs
    }

    #[test]
    fn find_resolves_a_distribution_in_any_account() {
        let mut accs = CloudFrontAccounts::new();
        accs.entry("111111111111")
            .distributions
            .insert("E1ABC".into(), dist("E1ABC", true, &[]));
        accs.entry("222222222222")
            .distributions
            .insert("E2DEF".into(), dist("E2DEF", true, &["cdn.example.com"]));
        assert_eq!(
            find_distribution_by_host(&accs, "e1abc.cloudfront.net").map(|(_, d)| d.id.as_str()),
            Some("E1ABC")
        );
        assert_eq!(
            find_distribution_by_host(&accs, "e2def.cloudfront.net").map(|(_, d)| d.id.as_str()),
            Some("E2DEF")
        );
        assert_eq!(
            find_distribution_by_host(&accs, "cdn.example.com").map(|(_, d)| d.id.as_str()),
            Some("E2DEF")
        );
    }

    #[test]
    fn find_by_domain_name() {
        let accs = accounts_with(vec![dist("E1ABC", true, &[])]);
        let (account, found) = find_distribution_by_host(&accs, "e1abc.cloudfront.net").unwrap();
        assert_eq!(found.id, "E1ABC");
        assert_eq!(account, "123456789012");
    }

    #[test]
    fn find_strips_port_and_is_case_insensitive() {
        let accs = accounts_with(vec![dist("E1ABC", true, &[])]);
        assert!(find_distribution_by_host(&accs, "E1ABC.CloudFront.net:4566").is_some());
    }

    #[test]
    fn find_by_alias_cname() {
        let accs = accounts_with(vec![dist("E1ABC", true, &["cdn.example.com"])]);
        let (_, found) = find_distribution_by_host(&accs, "cdn.example.com").unwrap();
        assert_eq!(found.id, "E1ABC");
    }

    #[test]
    fn disabled_distribution_is_not_matched() {
        let accs = accounts_with(vec![dist("E1ABC", false, &["cdn.example.com"])]);
        assert!(find_distribution_by_host(&accs, "e1abc.cloudfront.net").is_none());
        assert!(find_distribution_by_host(&accs, "cdn.example.com").is_none());
    }

    #[test]
    fn unknown_host_and_empty_host_return_none() {
        let accs = accounts_with(vec![dist("E1ABC", true, &[])]);
        assert!(find_distribution_by_host(&accs, "s3.amazonaws.com").is_none());
        assert!(find_distribution_by_host(&accs, "").is_none());
        assert!(find_distribution_by_host(&accs, ":4566").is_none());
    }

    #[test]
    fn s3_origin_detection_is_precise() {
        assert!(is_s3_origin("b.s3-website-us-east-1.amazonaws.com"));
        assert!(is_s3_origin("b.s3-website.us-east-1.amazonaws.com"));
        assert!(is_s3_origin("b.s3.us-east-1.amazonaws.com"));
        // A custom origin that merely contains the substring must NOT match.
        assert!(!is_s3_origin("my.s3-website.example.com"));
        assert!(!is_s3_origin("b.s3.us-east-1.amazonaws.com.example.com"));
        assert!(!is_s3_origin("api.example.com"));
        assert!(!is_s3_origin("127.0.0.1:8080"));
        // Path-style endpoints name no bucket, so there is nothing to serve.
        assert!(!is_s3_origin("s3.us-east-1.amazonaws.com"));
        assert!(!is_s3_origin("s3.amazonaws.com"));
        // Other AWS service hostnames are not S3 origins.
        assert!(!is_s3_origin("abc.execute-api.us-east-1.amazonaws.com"));
        assert!(!is_s3_origin("my-lb-1.us-east-1.elb.amazonaws.com"));
    }

    #[test]
    fn s3_website_origin_routes_to_local_port() {
        let up = upstream_for(
            &origin("b.s3-website-us-east-1.amazonaws.com", None),
            "127.0.0.1:4566",
        );
        assert_eq!(up.url_base, "http://127.0.0.1:4566");
        assert_eq!(up.host_header, "b.s3-website-us-east-1.amazonaws.com");
    }

    fn cfg_with_root(root: Option<&str>) -> DistributionConfig {
        DistributionConfig {
            default_root_object: root.map(Into::into),
            origins: Origins {
                quantity: 1,
                items: Some(OriginItems {
                    origin: vec![origin("b.s3.us-east-1.amazonaws.com", None)],
                }),
            },
            default_cache_behavior: DefaultCacheBehavior {
                target_origin_id: "o".into(),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn root_object_for(cfg: &DistributionConfig, path: &str) -> Option<String> {
        resolve_route(cfg, path, "127.0.0.1:4566", &no_ctx())
            .expect("route resolves")
            .root_object
    }

    #[test]
    fn default_root_object_applies_to_the_distribution_root() {
        let cfg = cfg_with_root(Some("index.html"));
        assert_eq!(root_object_for(&cfg, "/").as_deref(), Some("/index.html"));
        assert_eq!(root_object_for(&cfg, "").as_deref(), Some("/index.html"));
    }

    #[test]
    fn default_root_object_does_not_apply_below_the_root() {
        // AWS serves the default root object for the distribution root ONLY; a
        // subdirectory request is never rewritten to `<dir>/<object>`, and extra
        // slashes are left for the origin to interpret.
        let cfg = cfg_with_root(Some("index.html"));
        for path in ["/about/", "/about", "/index.html", "//", "///"] {
            assert_eq!(root_object_for(&cfg, path), None, "{path}");
        }
    }

    #[test]
    fn default_root_object_unset_or_empty_leaves_the_root_alone() {
        for root in [None, Some("")] {
            assert_eq!(root_object_for(&cfg_with_root(root), "/"), None, "{root:?}");
        }
    }

    #[test]
    fn default_root_object_path_is_used_as_given() {
        // A folder path is fetched from that folder.
        let cfg = cfg_with_root(Some("app/index.html"));
        assert_eq!(
            root_object_for(&cfg, "/").as_deref(),
            Some("/app/index.html")
        );
        // AWS does not strip a leading slash: the origin is asked for
        // `//index.html`, the documented cause of a 403 with such a value.
        let cfg = cfg_with_root(Some("/index.html"));
        assert_eq!(root_object_for(&cfg, "/").as_deref(), Some("//index.html"));
    }

    #[test]
    fn root_request_still_selects_the_cache_behavior_on_the_viewer_path() {
        // A behavior for the object's own path does not capture the root request.
        let mut cfg = cfg_with_root(Some("index.html"));
        cfg.origins.items.as_mut().unwrap().origin.push(Origin {
            id: "other".into(),
            ..origin("other.example.com", None)
        });
        cfg.cache_behaviors = Some(crate::model::CacheBehaviors {
            quantity: 1,
            items: Some(crate::model::CacheBehaviorItems {
                cache_behavior: vec![crate::model::CacheBehavior {
                    path_pattern: "index.html".into(),
                    target_origin_id: "other".into(),
                    ..Default::default()
                }],
            }),
        });
        let route = resolve_route(&cfg, "/", "127.0.0.1:4566", &no_ctx()).expect("route resolves");
        assert_eq!(route.root_object.as_deref(), Some("/index.html"));
        assert_eq!(route.upstream.host_header, "b.s3.us-east-1.amazonaws.com");
    }

    #[test]
    fn default_root_object_is_percent_encoded_for_the_origin_url() {
        // The value names an object literally: `#` and `?` must not become a
        // fragment or query, and space / `%` / non-ASCII must be escaped.
        for (root, want) in [
            ("report#v2.html", "/report%23v2.html"),
            ("a?b.html", "/a%3Fb.html"),
            ("my page.html", "/my%20page.html"),
            ("100%.html", "/100%25.html"),
            ("caf\u{e9}.html", "/caf%C3%A9.html"),
            ("app/index.html", "/app/index.html"),
            ("a-b_c.~!$&'()*+,;=:@.html", "/a-b_c.~!$&'()*+,;=:@.html"),
        ] {
            let cfg = cfg_with_root(Some(root));
            assert_eq!(root_object_for(&cfg, "/").as_deref(), Some(want), "{root}");
        }
    }

    fn cfg_with_origin_path(origin_path: Option<&str>, domain: &str) -> DistributionConfig {
        let mut cfg = cfg_with_root(Some("index.html"));
        let o = &mut cfg.origins.items.as_mut().unwrap().origin[0];
        o.domain_name = domain.into();
        o.origin_path = origin_path.map(Into::into);
        cfg
    }

    #[test]
    fn origin_path_prefixes_the_upstream_url_for_every_origin_kind() {
        for (domain, custom, base) in [
            (
                "site.s3-website-us-east-1.amazonaws.com",
                None,
                "http://127.0.0.1:4566",
            ),
            (
                "b.s3.us-east-1.amazonaws.com",
                None,
                "http://127.0.0.1:4566",
            ),
            (
                "api.example.com",
                Some(custom("https-only", 80, 8443)),
                "https://api.example.com:8443",
            ),
        ] {
            let mut cfg = cfg_with_origin_path(Some("/prod"), domain);
            cfg.origins.items.as_mut().unwrap().origin[0].custom_origin_config = custom;
            let route = resolve_route(&cfg, "/img/a.png", "127.0.0.1:4566", &no_ctx()).unwrap();
            assert_eq!(route.upstream.url_base, format!("{base}/prod"), "{domain}");
            assert_eq!(route.upstream.host_header, domain);
            // Custom error pages come from the default origin, prefixed too.
            assert_eq!(route.default_upstream.url_base, format!("{base}/prod"));
        }
    }

    #[test]
    fn origin_path_composes_with_the_default_root_object() {
        let cfg = cfg_with_origin_path(Some("/prod"), "b.s3.us-east-1.amazonaws.com");
        let route = resolve_route(&cfg, "/", "127.0.0.1:4566", &no_ctx()).unwrap();
        let url = format!("{}{}", route.upstream.url_base, route.root_object.unwrap());
        assert_eq!(url, "http://127.0.0.1:4566/prod/index.html");
    }

    #[test]
    fn origin_path_unset_or_empty_adds_nothing() {
        for p in [None, Some(""), Some("/")] {
            let cfg = cfg_with_origin_path(p, "b.s3.us-east-1.amazonaws.com");
            let route = resolve_route(&cfg, "/x", "127.0.0.1:4566", &no_ctx()).unwrap();
            assert_eq!(route.upstream.url_base, "http://127.0.0.1:4566", "{p:?}");
        }
    }

    #[test]
    fn origin_path_is_normalized_and_encoded() {
        for (p, want) in [
            ("/prod/", "/prod"),
            ("prod", "/prod"),
            ("/v 1/a#b", "/v%201/a%23b"),
            ("/a/b", "/a/b"),
        ] {
            let cfg = cfg_with_origin_path(Some(p), "b.s3.us-east-1.amazonaws.com");
            let route = resolve_route(&cfg, "/x", "127.0.0.1:4566", &no_ctx()).unwrap();
            assert_eq!(
                route.upstream.url_base,
                format!("http://127.0.0.1:4566{want}"),
                "{p}"
            );
        }
    }

    #[test]
    fn s3_rest_origin_routes_to_local_port() {
        // What an `S3OriginConfig` origin carries (CDK's `S3BucketOrigin` emits the
        // bucket's `RegionalDomainName`). These resolve in real DNS, so without
        // rerouting they are proxied to real AWS S3, which answers `NoSuchBucket`.
        for domain in [
            "b.s3.us-east-1.amazonaws.com",
            "b.s3.amazonaws.com",
            "b.s3-us-east-1.amazonaws.com",
            "my.dotted.bucket.s3.eu-west-2.amazonaws.com",
            "b.s3.dualstack.eu-west-2.amazonaws.com",
            "b.s3-fips.us-gov-west-1.amazonaws.com",
            "b.s3.cn-north-1.amazonaws.com.cn",
            "B.S3.US-EAST-1.AMAZONAWS.COM",
        ] {
            let up = upstream_for(&origin(domain, None), "127.0.0.1:4566");
            assert_eq!(up.url_base, "http://127.0.0.1:4566", "{domain}");
            assert_eq!(up.host_header, domain, "{domain}");
        }
    }

    #[test]
    fn every_bucket_endpoint_fakecloud_reports_is_served_locally() {
        // The hostnames CloudFormation reports for a bucket (DomainName,
        // RegionalDomainName, DualStackDomainName, WebsiteURL) come from the
        // same helpers; a template feeding any of them into an origin must be
        // served by this process in every partition and website form.
        use fakecloud_aws::endpoint;
        for region in [
            "us-east-1",
            "eu-central-1",
            "us-gov-west-1",
            "us-gov-east-1",
            "cn-north-1",
            "us-iso-east-1",
            "us-isob-east-1",
            "us-isof-south-1",
            "eu-isoe-west-1",
        ] {
            let website = endpoint::s3_website_url("my.site", region);
            for domain in [
                endpoint::s3_bucket_domain_name("my.site", region),
                endpoint::s3_regional_domain_name("my.site", region),
                endpoint::s3_dualstack_domain_name("my.site", region),
                website.trim_start_matches("http://").to_string(),
            ] {
                let up = upstream_for(&origin(&domain, None), "127.0.0.1:4566");
                assert_eq!(up.url_base, "http://127.0.0.1:4566", "{domain}");
                let host = fakecloud_core::protocol::parse_routing_host(&domain).unwrap();
                assert_eq!(host.bucket.as_deref(), Some("my.site"), "{domain}");
            }
        }
    }

    #[test]
    fn s3_lookalike_origin_is_not_rerouted() {
        // Not an AWS hostname: a real custom origin that must keep its own domain.
        let up = upstream_for(&origin("s3.example.com", None), "127.0.0.1:4566");
        assert_eq!(up.url_base, "http://s3.example.com");
    }

    #[test]
    fn s3_origin_with_custom_origin_config_is_still_served_locally() {
        // A bucket REST endpoint may be declared as a custom origin (e.g. CDK's
        // `HttpOrigin(bucket.bucketRegionalDomainName)`), and a website endpoint
        // always is. The bucket lives in this process either way; honoring the
        // config would send the fetch to real AWS S3.
        for domain in [
            "b.s3.us-east-1.amazonaws.com",
            "b.s3-website.eu-central-1.amazonaws.com",
        ] {
            let up = upstream_for(
                &origin(domain, Some(custom("https-only", 80, 8443))),
                "127.0.0.1:4566",
            );
            assert_eq!(up.url_base, "http://127.0.0.1:4566", "{domain}");
            assert_eq!(up.host_header, domain, "{domain}");
        }
    }

    #[test]
    fn https_only_custom_origin_uses_https_and_port() {
        let up = upstream_for(
            &origin("api.example.com", Some(custom("https-only", 80, 8443))),
            "127.0.0.1:4566",
        );
        assert_eq!(up.url_base, "https://api.example.com:8443");
    }

    #[test]
    fn http_custom_origin_default_port_omits_port() {
        let up = upstream_for(
            &origin("api.example.com", Some(custom("http-only", 80, 443))),
            "127.0.0.1:4566",
        );
        assert_eq!(up.url_base, "http://api.example.com");
    }

    #[test]
    fn explicit_port_in_domain_wins_over_config_port() {
        // Local origins encode the port in the domain; the config port (80) must
        // not be appended on top of it.
        let up = upstream_for(
            &origin("127.0.0.1:52111", Some(custom("http-only", 80, 443))),
            "127.0.0.1:4566",
        );
        assert_eq!(up.url_base, "http://127.0.0.1:52111");
    }

    #[test]
    fn bare_origin_defaults_to_http() {
        let up = upstream_for(&origin("origin.internal", None), "127.0.0.1:4566");
        assert_eq!(up.url_base, "http://origin.internal");
    }

    // ── Origin access (OAC / OAI) ────────────────────────────────────────

    const DIST_ARN: &str = "arn:aws:cloudfront::123456789012:distribution/E1ABC";

    fn account_with_access(oac: Option<(&str, &str, &str)>, oai: Option<&str>) -> AccountState {
        let mut account = AccountState::default();
        if let Some((id, behavior, origin_type)) = oac {
            account.origin_access_controls.insert(
                id.to_string(),
                crate::policies::StoredOriginAccessControl {
                    id: id.to_string(),
                    etag: "E".into(),
                    config: crate::policies::OriginAccessControlConfig {
                        name: "oac".into(),
                        description: None,
                        signing_protocol: "sigv4".into(),
                        signing_behavior: behavior.into(),
                        origin_access_control_origin_type: origin_type.into(),
                    },
                },
            );
        }
        if let Some(id) = oai {
            account.origin_access_identities.insert(
                id.to_string(),
                crate::functions::StoredOriginAccessIdentity {
                    id: id.to_string(),
                    etag: "E".into(),
                    s3_canonical_user_id: "0123456789abcdef0123456789abcdef".into(),
                    config: Default::default(),
                },
            );
        }
        account
    }

    fn ctx_for<'a>(arn: &'a str, account: &'a AccountState) -> RouteContext<'a> {
        RouteContext {
            distribution_arn: arn,
            account_id: "123456789012",
            account: Some(account),
        }
    }

    fn s3_origin(domain: &str, oac: Option<&str>, oai: Option<&str>) -> Origin {
        Origin {
            id: "o".into(),
            domain_name: domain.into(),
            origin_access_control_id: oac.map(str::to_string),
            s3_origin_config: Some(crate::model::S3OriginConfig {
                origin_access_identity: oai.unwrap_or_default().to_string(),
                origin_read_timeout: None,
            }),
            ..Default::default()
        }
    }

    fn cloudfront_caller() -> InternalCaller {
        InternalCaller::Service {
            service: "cloudfront.amazonaws.com".into(),
            source_arn: DIST_ARN.into(),
            source_account: "123456789012".into(),
        }
    }

    #[test]
    fn oac_signs_as_the_cloudfront_service_principal_per_signing_behavior() {
        let origin = s3_origin("b.s3.us-east-1.amazonaws.com", Some("OAC1"), None);
        for (behavior, want) in [
            ("always", OriginAuth::Always(cloudfront_caller())),
            ("no-override", OriginAuth::NoOverride(cloudfront_caller())),
            ("never", OriginAuth::Anonymous),
        ] {
            let account = account_with_access(Some(("OAC1", behavior, "s3")), None);
            let target = origin_target(&origin, "127.0.0.1:4566", &ctx_for(DIST_ARN, &account));
            assert!(target.local);
            assert_eq!(target.auth, want, "{behavior}");
        }
    }

    #[test]
    fn oac_that_is_missing_or_not_for_s3_leaves_the_fetch_unsigned() {
        let origin = s3_origin("b.s3.us-east-1.amazonaws.com", Some("OAC1"), None);
        let none = AccountState::default();
        assert_eq!(
            origin_auth(&origin, &ctx_for(DIST_ARN, &none)),
            OriginAuth::Anonymous
        );
        let mediastore = account_with_access(Some(("OAC1", "always", "mediastore")), None);
        assert_eq!(
            origin_auth(&origin, &ctx_for(DIST_ARN, &mediastore)),
            OriginAuth::Anonymous
        );
    }

    #[test]
    fn oai_signs_as_the_origin_access_identity() {
        let origin = s3_origin(
            "b.s3.cn-north-1.amazonaws.com.cn",
            None,
            Some("origin-access-identity/cloudfront/E2QWRUHAPOMQZL"),
        );
        let account = account_with_access(None, Some("E2QWRUHAPOMQZL"));
        let arn = "arn:aws-cn:cloudfront::123456789012:distribution/E1ABC";
        assert_eq!(
            origin_auth(&origin, &ctx_for(arn, &account)),
            OriginAuth::Always(InternalCaller::ServiceOwned {
                arn: "arn:aws-cn:iam::cloudfront:user/CloudFront Origin Access Identity E2QWRUHAPOMQZL"
                    .into(),
                canonical_user_id: Some("0123456789abcdef0123456789abcdef".into()),
                acting_account: "123456789012".into(),
            })
        );
        // An OAI the account does not hold never grants.
        let empty = AccountState::default();
        assert_eq!(
            origin_auth(&origin, &ctx_for(arn, &empty)),
            OriginAuth::Anonymous
        );
    }

    #[test]
    fn oac_wins_over_a_leftover_oai() {
        let origin = s3_origin(
            "b.s3.us-east-1.amazonaws.com",
            Some("OAC1"),
            Some("origin-access-identity/cloudfront/E2QWRUHAPOMQZL"),
        );
        let account = account_with_access(Some(("OAC1", "always", "s3")), Some("E2QWRUHAPOMQZL"));
        assert_eq!(
            origin_auth(&origin, &ctx_for(DIST_ARN, &account)),
            OriginAuth::Always(cloudfront_caller())
        );
    }

    #[test]
    fn origin_without_access_config_is_anonymous() {
        let origin = s3_origin("b.s3.us-east-1.amazonaws.com", None, None);
        let account = account_with_access(Some(("OAC1", "always", "s3")), Some("E1"));
        assert_eq!(
            origin_auth(&origin, &ctx_for(DIST_ARN, &account)),
            OriginAuth::Anonymous
        );
    }

    #[test]
    fn website_endpoints_and_custom_origins_are_never_signed() {
        let account = account_with_access(Some(("OAC1", "always", "s3")), None);
        let website = s3_origin("b.s3-website-us-east-1.amazonaws.com", Some("OAC1"), None);
        let target = origin_target(&website, "127.0.0.1:4566", &ctx_for(DIST_ARN, &account));
        assert!(target.local);
        assert_eq!(target.auth, OriginAuth::Anonymous);
        let dotted = s3_origin("b.s3-website.eu-west-1.amazonaws.com", Some("OAC1"), None);
        assert_eq!(
            origin_auth(&dotted, &ctx_for(DIST_ARN, &account)),
            OriginAuth::Anonymous
        );
        let mut custom_origin = origin("api.example.com", Some(custom("https-only", 80, 443)));
        custom_origin.origin_access_control_id = Some("OAC1".into());
        let target = origin_target(
            &custom_origin,
            "127.0.0.1:4566",
            &ctx_for(DIST_ARN, &account),
        );
        assert!(!target.local);
        assert_eq!(target.auth, OriginAuth::Anonymous);
    }

    #[test]
    fn website_detection_requires_the_website_endpoint() {
        assert!(is_s3_website_origin("b.s3-website-us-east-1.amazonaws.com"));
        assert!(is_s3_website_origin("B.S3-Website.us-east-1.amazonaws.com"));
        assert!(!is_s3_website_origin("b.s3.us-east-1.amazonaws.com"));
        // A dotted bucket whose name merely contains "s3-website".
        assert!(!is_s3_website_origin(
            "my.s3-website.bucket.s3.us-east-1.amazonaws.com"
        ));
        assert!(!is_s3_website_origin("api.example.com"));
    }

    #[test]
    fn no_override_defers_to_a_viewer_authorization_header() {
        let auth = OriginAuth::NoOverride(cloudfront_caller());
        assert_eq!(
            auth.caller_for(&HeaderMap::new()),
            Some(&cloudfront_caller())
        );
        let mut viewer = HeaderMap::new();
        viewer.insert(header::AUTHORIZATION, "AWS4-HMAC-SHA256 x".parse().unwrap());
        assert_eq!(auth.caller_for(&viewer), None);
        // `always` replaces it.
        let always = OriginAuth::Always(cloudfront_caller());
        assert_eq!(always.caller_for(&viewer), Some(&cloudfront_caller()));
        assert_eq!(OriginAuth::Anonymous.caller_for(&HeaderMap::new()), None);
    }

    #[test]
    fn local_request_target_is_percent_encoded_not_dropped() {
        // A custom error page path with a space is joined raw; the in-process
        // fetch must request that object, not fall back to the bucket root.
        assert_eq!(
            local_path_and_query("http://127.0.0.1:4566/errors/not found.html").as_deref(),
            Some("/errors/not%20found.html")
        );
        assert_eq!(
            local_path_and_query("http://127.0.0.1:4566/prod/a.png?v=1").as_deref(),
            Some("/prod/a.png?v=1")
        );
    }
}
