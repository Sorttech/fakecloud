//! Handlers for CloudFront Streaming Distributions (legacy RTMP).

use chrono::Utc;
use http::header::{ETAG, LOCATION};
use http::{HeaderMap, HeaderValue, StatusCode};
use uuid::Uuid;

use fakecloud_core::service::{AwsRequest, AwsResponse, AwsServiceError};

use crate::policies::{
    not_found, precondition_failed, require_if_match, rfc3339, route_id, xml_with_etag,
};
use crate::router::Route;
use crate::service::{
    aws_error, esc, generate_etag, invalid_argument, xml_response, CloudFrontService,
};
use crate::state::Tag;
use crate::streaming::{
    StoredStreamingDistribution, StreamingDistributionConfig, StreamingDistributionConfigWithTags,
};
use crate::xml_io;

const NS: &str = crate::NAMESPACE;
const XML_DECL: &str = r#"<?xml version="1.0" encoding="UTF-8"?>"#;

impl CloudFrontService {
    pub(crate) fn create_streaming_distribution(
        &self,
        req: &AwsRequest,
        with_tags: bool,
    ) -> Result<AwsResponse, AwsServiceError> {
        let (config, tags) = if with_tags {
            let parsed: StreamingDistributionConfigWithTags = xml_io::from_xml_root(&req.body)
                .map_err(|e| {
                    invalid_argument(format!(
                        "invalid StreamingDistributionConfigWithTags XML: {e}"
                    ))
                })?;
            let tags = parsed
                .tags
                .items
                .map(|i| {
                    i.tag
                        .into_iter()
                        .map(|t| Tag {
                            key: t.key,
                            value: t.value,
                        })
                        .collect()
                })
                .unwrap_or_default();
            (parsed.streaming_distribution_config, tags)
        } else {
            let parsed: StreamingDistributionConfig =
                xml_io::from_xml_root(&req.body).map_err(|e| {
                    invalid_argument(format!("invalid StreamingDistributionConfig XML: {e}"))
                })?;
            (parsed, Vec::new())
        };
        if config.caller_reference.is_empty() {
            return Err(invalid_argument("CallerReference is required"));
        }

        let mut state = self.state.write();
        let id = state.unused_id(generate_streaming_id, |a, id| {
            a.streaming_distributions.contains_key(id)
        });
        let account = state.accounts.entry(req.account_id.clone()).or_default();

        if let Some(existing) = account
            .streaming_distributions
            .values()
            .find(|d| d.config.caller_reference == config.caller_reference)
        {
            return Err(aws_error(
                StatusCode::CONFLICT,
                "StreamingDistributionAlreadyExists",
                format!(
                    "Streaming distribution with same CallerReference exists: {}",
                    existing.id
                ),
            ));
        }

        let now = Utc::now();
        let etag = generate_etag();
        let domain = format!("{}.cloudfront.net", id.to_lowercase());
        let arn = crate::service::cloudfront_arn(
            &req.region,
            &req.account_id,
            &format!("streaming-distribution/{id}"),
        );

        let stored = StoredStreamingDistribution {
            id: id.clone(),
            arn: arn.clone(),
            // Real CloudFront returns `InProgress` for streaming
            // distributions just like regular distributions and flips to
            // `Deployed` once edge propagation completes.
            status: "InProgress".to_string(),
            last_modified_time: now,
            domain_name: domain,
            etag: etag.clone(),
            config,
        };
        account
            .streaming_distributions
            .insert(id.clone(), stored.clone());
        if !tags.is_empty() {
            account.tags.insert(arn.clone(), tags);
        }
        drop(state);

        self.schedule_streaming_distribution_deploy(id.clone());

        let body = render_streaming_distribution(&stored);
        let mut headers = HeaderMap::new();
        if let Ok(v) = HeaderValue::from_str(&etag) {
            headers.insert(ETAG, v);
        }
        if let Ok(v) = HeaderValue::from_str(&stored.arn) {
            headers.insert(LOCATION, v);
        }
        Ok(xml_response(StatusCode::CREATED, body, headers))
    }

    pub(crate) fn get_streaming_distribution(
        &self,
        route: &Route,
    ) -> Result<AwsResponse, AwsServiceError> {
        let id = route_id(route, "StreamingDistribution")?;
        let state = self.state.read();
        let d = state
            .accounts
            .get(&route.account)
            .and_then(|a| a.streaming_distributions.get(&id).cloned())
            .ok_or_else(|| not_found("StreamingDistribution", &id))?;
        drop(state);
        let body = render_streaming_distribution(&d);
        Ok(xml_with_etag(StatusCode::OK, body, &d.etag, None))
    }

    pub(crate) fn get_streaming_distribution_config(
        &self,
        route: &Route,
    ) -> Result<AwsResponse, AwsServiceError> {
        let id = route_id(route, "StreamingDistribution")?;
        let state = self.state.read();
        let d = state
            .accounts
            .get(&route.account)
            .and_then(|a| a.streaming_distributions.get(&id).cloned())
            .ok_or_else(|| not_found("StreamingDistribution", &id))?;
        drop(state);
        let body = render_streaming_distribution_config(&d.config);
        Ok(xml_with_etag(StatusCode::OK, body, &d.etag, None))
    }

    pub(crate) fn update_streaming_distribution(
        &self,
        req: &AwsRequest,
        route: &Route,
    ) -> Result<AwsResponse, AwsServiceError> {
        let id = route_id(route, "StreamingDistribution")?;
        let if_match = require_if_match(req)?;
        let cfg: StreamingDistributionConfig = xml_io::from_xml_root(&req.body).map_err(|e| {
            invalid_argument(format!("invalid StreamingDistributionConfig XML: {e}"))
        })?;
        if cfg.caller_reference.is_empty() {
            return Err(invalid_argument("CallerReference is required"));
        }

        let mut state = self.state.write();
        let account = state
            .accounts
            .get_mut(&req.account_id)
            .ok_or_else(|| not_found("StreamingDistribution", &id))?;
        let d = account
            .streaming_distributions
            .get_mut(&id)
            .ok_or_else(|| not_found("StreamingDistribution", &id))?;
        if d.etag != if_match {
            return Err(precondition_failed());
        }
        if d.config.caller_reference != cfg.caller_reference {
            return Err(invalid_argument(
                "CallerReference cannot change on UpdateStreamingDistribution",
            ));
        }
        // ETag stability: only bump when the config actually changes. A
        // no-op UpdateStreamingDistribution leaves the ETag and status
        // untouched, matching real CloudFront.
        let config_changed = !streaming_configs_equal(&d.config, &cfg);
        if config_changed {
            d.config = cfg;
            d.etag = generate_etag();
            d.last_modified_time = Utc::now();
            // UpdateStreamingDistribution kicks off fresh edge
            // propagation; AWS flips status back to `InProgress` until
            // the new config lands.
            d.status = "InProgress".to_string();
        }
        let snap = d.clone();
        drop(state);

        if config_changed {
            self.schedule_streaming_distribution_deploy(id.clone());
        }

        let body = render_streaming_distribution(&snap);
        Ok(xml_with_etag(StatusCode::OK, body, &snap.etag, None))
    }

    pub(crate) fn delete_streaming_distribution(
        &self,
        req: &AwsRequest,
        route: &Route,
    ) -> Result<AwsResponse, AwsServiceError> {
        let id = route_id(route, "StreamingDistribution")?;
        let if_match = require_if_match(req)?;
        let mut state = self.state.write();
        let account = state
            .accounts
            .get_mut(&req.account_id)
            .ok_or_else(|| not_found("StreamingDistribution", &id))?;
        let d = account
            .streaming_distributions
            .get(&id)
            .ok_or_else(|| not_found("StreamingDistribution", &id))?;
        if d.etag != if_match {
            return Err(precondition_failed());
        }
        if d.config.enabled {
            return Err(aws_error(
                StatusCode::PRECONDITION_FAILED,
                "StreamingDistributionNotDisabled",
                "StreamingDistribution must be disabled before deletion",
            ));
        }
        let arn = d.arn.clone();
        account.streaming_distributions.remove(&id);
        // Tags are keyed by ARN — drop them too so ListTagsForResource
        // doesn't return tags for a deleted resource.
        account.tags.remove(&arn);
        drop(state);
        Ok(crate::policies::empty(StatusCode::NO_CONTENT))
    }

    pub(crate) fn list_streaming_distributions(
        &self,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        let state = self.state.read();
        let mut items: Vec<StoredStreamingDistribution> = state
            .accounts
            .get(&req.account_id)
            .map(|a| a.streaming_distributions.values().cloned().collect())
            .unwrap_or_default();
        drop(state);
        items.sort_by(|a, b| a.id.cmp(&b.id));

        let mut body = String::with_capacity(512);
        body.push_str(XML_DECL);
        body.push_str(&format!("<StreamingDistributionList xmlns=\"{NS}\">"));
        body.push_str("<Marker></Marker>");
        body.push_str("<MaxItems>100</MaxItems>");
        body.push_str("<IsTruncated>false</IsTruncated>");
        body.push_str(&format!("<Quantity>{}</Quantity>", items.len()));
        body.push_str("<Items>");
        for d in &items {
            body.push_str("<StreamingDistributionSummary>");
            push_summary_inner(&mut body, d);
            body.push_str("</StreamingDistributionSummary>");
        }
        body.push_str("</Items>");
        body.push_str("</StreamingDistributionList>");
        Ok(xml_response(StatusCode::OK, body, HeaderMap::new()))
    }
}

// ─── Helpers ──────────────────────────────────────────────────────────

fn generate_streaming_id() -> String {
    let raw = Uuid::new_v4().simple().to_string().to_uppercase();
    format!("S{}", &raw[..13])
}

fn render_streaming_distribution(d: &StoredStreamingDistribution) -> String {
    let mut out = String::with_capacity(512);
    out.push_str(XML_DECL);
    out.push_str(&format!("<StreamingDistribution xmlns=\"{NS}\">"));
    out.push_str(&format!("<Id>{}</Id>", esc(&d.id)));
    out.push_str(&format!("<ARN>{}</ARN>", esc(&d.arn)));
    out.push_str(&format!("<Status>{}</Status>", esc(&d.status)));
    out.push_str(&format!(
        "<LastModifiedTime>{}</LastModifiedTime>",
        rfc3339(&d.last_modified_time)
    ));
    out.push_str(&format!("<DomainName>{}</DomainName>", esc(&d.domain_name)));
    out.push_str(&render_active_trusted_signers(&d.config));
    out.push_str(&serialize_config_inner(&d.config));
    out.push_str("</StreamingDistribution>");
    out
}

fn render_streaming_distribution_config(cfg: &StreamingDistributionConfig) -> String {
    let mut out = String::with_capacity(512);
    out.push_str(XML_DECL);
    out.push_str(&format!("<StreamingDistributionConfig xmlns=\"{NS}\">"));
    out.push_str(&serialize_config_body(cfg));
    out.push_str("</StreamingDistributionConfig>");
    out
}

fn push_summary_inner(out: &mut String, d: &StoredStreamingDistribution) {
    out.push_str(&format!("<Id>{}</Id>", esc(&d.id)));
    out.push_str(&format!("<ARN>{}</ARN>", esc(&d.arn)));
    out.push_str(&format!("<Status>{}</Status>", esc(&d.status)));
    out.push_str(&format!(
        "<LastModifiedTime>{}</LastModifiedTime>",
        rfc3339(&d.last_modified_time)
    ));
    out.push_str(&format!("<DomainName>{}</DomainName>", esc(&d.domain_name)));
    // S3Origin
    out.push_str("<S3Origin>");
    out.push_str(&format!(
        "<DomainName>{}</DomainName>",
        esc(&d.config.s3_origin.domain_name)
    ));
    out.push_str(&format!(
        "<OriginAccessIdentity>{}</OriginAccessIdentity>",
        esc(&d.config.s3_origin.origin_access_identity)
    ));
    out.push_str("</S3Origin>");
    out.push_str(&render_aliases(&d.config));
    out.push_str(&render_trusted_signers(&d.config));
    out.push_str(&format!("<Comment>{}</Comment>", esc(&d.config.comment)));
    out.push_str(&format!(
        "<PriceClass>{}</PriceClass>",
        esc(&d.config.price_class)
    ));
    out.push_str(&format!("<Enabled>{}</Enabled>", d.config.enabled));
}

fn serialize_config_inner(cfg: &StreamingDistributionConfig) -> String {
    let mut out = String::with_capacity(512);
    out.push_str("<StreamingDistributionConfig>");
    out.push_str(&serialize_config_body(cfg));
    out.push_str("</StreamingDistributionConfig>");
    out
}

fn serialize_config_body(cfg: &StreamingDistributionConfig) -> String {
    let mut out = String::with_capacity(512);
    out.push_str(&format!(
        "<CallerReference>{}</CallerReference>",
        esc(&cfg.caller_reference)
    ));
    out.push_str("<S3Origin>");
    out.push_str(&format!(
        "<DomainName>{}</DomainName>",
        esc(&cfg.s3_origin.domain_name)
    ));
    out.push_str(&format!(
        "<OriginAccessIdentity>{}</OriginAccessIdentity>",
        esc(&cfg.s3_origin.origin_access_identity)
    ));
    out.push_str("</S3Origin>");
    out.push_str(&render_aliases(cfg));
    out.push_str(&format!("<Comment>{}</Comment>", esc(&cfg.comment)));
    if let Some(log) = &cfg.logging {
        out.push_str("<Logging>");
        out.push_str(&format!("<Enabled>{}</Enabled>", log.enabled));
        out.push_str(&format!("<Bucket>{}</Bucket>", esc(&log.bucket)));
        out.push_str(&format!("<Prefix>{}</Prefix>", esc(&log.prefix)));
        out.push_str("</Logging>");
    }
    out.push_str(&render_trusted_signers(cfg));
    out.push_str(&format!(
        "<PriceClass>{}</PriceClass>",
        esc(&cfg.price_class)
    ));
    out.push_str(&format!("<Enabled>{}</Enabled>", cfg.enabled));
    out
}

fn render_aliases(cfg: &StreamingDistributionConfig) -> String {
    let mut out = String::with_capacity(64);
    let (qty, items) = match &cfg.aliases {
        Some(a) => (a.quantity, a.items.clone()),
        None => (0, None),
    };
    out.push_str("<Aliases>");
    out.push_str(&format!("<Quantity>{}</Quantity>", qty));
    if let Some(it) = items {
        out.push_str("<Items>");
        for c in &it.cname {
            out.push_str(&format!("<CNAME>{}</CNAME>", esc(c)));
        }
        out.push_str("</Items>");
    }
    out.push_str("</Aliases>");
    out
}

fn render_trusted_signers(cfg: &StreamingDistributionConfig) -> String {
    let mut out = String::with_capacity(64);
    out.push_str("<TrustedSigners>");
    out.push_str(&format!(
        "<Enabled>{}</Enabled>",
        cfg.trusted_signers.enabled
    ));
    out.push_str(&format!(
        "<Quantity>{}</Quantity>",
        cfg.trusted_signers.quantity
    ));
    if let Some(it) = &cfg.trusted_signers.items {
        out.push_str("<Items>");
        for a in &it.aws_account_number {
            out.push_str(&format!("<AwsAccountNumber>{}</AwsAccountNumber>", esc(a)));
        }
        out.push_str("</Items>");
    }
    out.push_str("</TrustedSigners>");
    out
}

fn render_active_trusted_signers(cfg: &StreamingDistributionConfig) -> String {
    let mut out = String::with_capacity(64);
    out.push_str("<ActiveTrustedSigners>");
    out.push_str(&format!(
        "<Enabled>{}</Enabled>",
        cfg.trusted_signers.enabled
    ));
    out.push_str(&format!(
        "<Quantity>{}</Quantity>",
        cfg.trusted_signers.quantity
    ));
    if let Some(it) = &cfg.trusted_signers.items {
        out.push_str("<Items>");
        for a in &it.aws_account_number {
            out.push_str("<Signer>");
            out.push_str(&format!("<AwsAccountNumber>{}</AwsAccountNumber>", esc(a)));
            out.push_str("</Signer>");
        }
        out.push_str("</Items>");
    }
    out.push_str("</ActiveTrustedSigners>");
    out
}

/// Compare two `StreamingDistributionConfig`s by their canonical XML
/// representation. Mirrors `service::configs_equal` for the streaming
/// (RTMP) distribution variant; used so a no-op `UpdateStreamingDistribution`
/// leaves the ETag stable.
fn streaming_configs_equal(
    lhs: &StreamingDistributionConfig,
    rhs: &StreamingDistributionConfig,
) -> bool {
    let Ok(a) = xml_io::to_xml_root("StreamingDistributionConfig", lhs) else {
        return false;
    };
    let Ok(b) = xml_io::to_xml_root("StreamingDistributionConfig", rhs) else {
        return false;
    };
    a == b
}
