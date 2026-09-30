//! OTel enrichment (start/stop/update with metric filters), alarm contributors, and metric widget image.

use chrono::{DateTime, Utc};
use fakecloud_core::query::optional_query_param;
use fakecloud_core::service::{AwsRequest, AwsResponse, AwsServiceError};

use crate::service::{
    collect_indexed, collect_member_strings, missing_param, not_found, validation_error,
    xml_escape, xml_response, CloudWatchService,
};
use crate::state::{OTelEnrichmentConfig, OTelMetricSelector};

/// A tiny, valid 1x1 transparent PNG (raw bytes). GetMetricWidgetImage
/// returns a `MetricWidgetImage` blob; this is a deterministic placeholder
/// image rather than a stubbed string.
const TINY_PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4,
    0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00, 0x01, 0x00, 0x00,
    0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE,
    0x42, 0x60, 0x82,
];

/// At most 100 selectors across `IncludeFilters` and `ExcludeFilters`.
const MAX_OTEL_FILTERS: usize = 100;
/// At most 100 metric names per selector.
const MAX_OTEL_METRIC_NAMES: usize = 100;

/// Parse a `<prefix>.member.N` list of `OTelEnrichmentMetricSelector`s,
/// validating each selector against the model constraints.
fn parse_selectors(
    req: &AwsRequest,
    prefix: &str,
) -> Result<Vec<OTelMetricSelector>, AwsServiceError> {
    let mut out = Vec::new();
    for member in collect_indexed(req, prefix) {
        let namespace = member
            .get("Namespace")
            .cloned()
            .ok_or_else(|| validation_error(format!("{prefix}.Namespace is required")))?;
        let ns_len = namespace.chars().count();
        if !(1..=255).contains(&ns_len) || namespace.starts_with(':') {
            return Err(validation_error(format!(
                "{prefix}.Namespace '{namespace}' is invalid: must be 1-255 characters and not start with ':'"
            )));
        }
        let metric_names = collect_member_strings(&member, "MetricNames");
        if metric_names.len() > MAX_OTEL_METRIC_NAMES {
            return Err(validation_error(format!(
                "A maximum of {MAX_OTEL_METRIC_NAMES} metric names is allowed for each selector"
            )));
        }
        if let Some(bad) = metric_names
            .iter()
            .find(|n| !(1..=255).contains(&n.chars().count()))
        {
            return Err(validation_error(format!(
                "Metric name '{bad}' must be 1-255 characters"
            )));
        }
        out.push(OTelMetricSelector {
            namespace,
            metric_names,
        });
    }
    Ok(out)
}

/// Parse and validate the include/exclude filter pair of a Start/Update call.
fn parse_filter_pair(
    req: &AwsRequest,
) -> Result<(Vec<OTelMetricSelector>, Vec<OTelMetricSelector>), AwsServiceError> {
    let include = parse_selectors(req, "IncludeFilters")?;
    let exclude = parse_selectors(req, "ExcludeFilters")?;
    if include.len() + exclude.len() > MAX_OTEL_FILTERS {
        return Err(validation_error(format!(
            "A maximum of {MAX_OTEL_FILTERS} filters is allowed across IncludeFilters and ExcludeFilters combined"
        )));
    }
    Ok((include, exclude))
}

fn render_selectors(out: &mut String, tag: &str, selectors: &[OTelMetricSelector]) {
    if selectors.is_empty() {
        return;
    }
    out.push_str(&format!("<{tag}>"));
    for sel in selectors {
        out.push_str("<member>");
        out.push_str(&format!(
            "<Namespace>{}</Namespace>",
            xml_escape(&sel.namespace)
        ));
        if !sel.metric_names.is_empty() {
            out.push_str("<MetricNames>");
            for n in &sel.metric_names {
                out.push_str(&format!("<member>{}</member>", xml_escape(n)));
            }
            out.push_str("</MetricNames>");
        }
        out.push_str("</member>");
    }
    out.push_str(&format!("</{tag}>"));
}

/// Render the stored filters plus CreatedAt/UpdatedAt, omitting each empty
/// filter list (an omitted list means "all namespaces" / "nothing excluded").
fn render_config(cfg: &OTelEnrichmentConfig) -> String {
    let mut inner = String::new();
    render_selectors(&mut inner, "IncludeFilters", &cfg.include_filters);
    render_selectors(&mut inner, "ExcludeFilters", &cfg.exclude_filters);
    if let Some(t) = cfg.created_at {
        inner.push_str(&format!("<CreatedAt>{}</CreatedAt>", fmt_ts(t)));
    }
    if let Some(t) = cfg.updated_at {
        inner.push_str(&format!("<UpdatedAt>{}</UpdatedAt>", fmt_ts(t)));
    }
    inner
}

pub(crate) fn fmt_ts(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

impl CloudWatchService {
    pub(crate) fn get_otel_enrichment(
        &self,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        let state = self.state.read();
        let acct = state.get(&req.account_id);
        let running = acct.map(|a| a.otel_enrichment_running).unwrap_or(false);
        // Filters and CreatedAt are only reported while enrichment runs.
        let inner = match acct {
            Some(a) if running => format!(
                "<Status>Running</Status>{}",
                render_config(&a.otel_enrichment)
            ),
            _ => "<Status>Stopped</Status>".to_string(),
        };
        Ok(xml_response("GetOTelEnrichment", &inner, &req.request_id))
    }

    pub(crate) fn start_otel_enrichment(
        &self,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        let (include, exclude) = parse_filter_pair(req)?;
        let mut state = self.state.write();
        let acct = state.get_or_create(&req.account_id);
        // Starting an already-running account is a no-op: the stored filters
        // are kept and only UpdateOTelEnrichment can change them.
        if !acct.otel_enrichment_running {
            let now = Utc::now();
            acct.otel_enrichment_running = true;
            acct.otel_enrichment = OTelEnrichmentConfig {
                include_filters: include,
                exclude_filters: exclude,
                created_at: Some(now),
                updated_at: Some(now),
            };
        }
        let inner = render_config(&acct.otel_enrichment);
        Ok(xml_response("StartOTelEnrichment", &inner, &req.request_id))
    }

    pub(crate) fn update_otel_enrichment(
        &self,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        let (include, exclude) = parse_filter_pair(req)?;
        let mut state = self.state.write();
        let acct = state.get_or_create(&req.account_id);
        if !acct.otel_enrichment_running {
            return Err(not_found(
                "OTel enrichment is not running for this account. Call StartOTelEnrichment first.",
            ));
        }
        // Include and exclude are replaced as a pair: omitting one clears it.
        acct.otel_enrichment.include_filters = include;
        acct.otel_enrichment.exclude_filters = exclude;
        acct.otel_enrichment.updated_at = Some(Utc::now());
        let inner = render_config(&acct.otel_enrichment);
        Ok(xml_response(
            "UpdateOTelEnrichment",
            &inner,
            &req.request_id,
        ))
    }

    pub(crate) fn stop_otel_enrichment(
        &self,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        let mut state = self.state.write();
        let acct = state.get_or_create(&req.account_id);
        acct.otel_enrichment_running = false;
        acct.otel_enrichment = OTelEnrichmentConfig::default();
        Ok(xml_response("StopOTelEnrichment", "", &req.request_id))
    }

    pub(crate) fn describe_alarm_contributors(
        &self,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        // AlarmName is a required parameter: an omitted name is a missing-
        // parameter validation error, whereas a present-but-unknown alarm is
        // the declared ResourceNotFoundException.
        let alarm_name =
            optional_query_param(req, "AlarmName").ok_or_else(|| missing_param("AlarmName"))?;
        let state = self.state.read();
        let exists = state
            .get(&req.account_id)
            .map(|a| {
                a.alarms_in(&req.region)
                    .map(|m| m.contains_key(&alarm_name))
                    .unwrap_or(false)
                    || a.composite_alarms_in(&req.region)
                        .map(|m| m.contains_key(&alarm_name))
                        .unwrap_or(false)
            })
            .unwrap_or(false);
        if !exists {
            return Err(not_found(format!("Alarm {alarm_name} does not exist")));
        }
        // No live contributor evaluation; return an empty contributor list.
        let inner = String::from("<AlarmContributors/>");
        Ok(xml_response(
            "DescribeAlarmContributors",
            &inner,
            &req.request_id,
        ))
    }

    pub(crate) fn get_metric_widget_image(
        &self,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        // No declared errors, but the omitted-required negative variant still
        // expects a 4xx (accepted as AnyError). A well-formed request returns a
        // deterministic image.
        if optional_query_param(req, "MetricWidget").is_none() {
            return Err(missing_param("MetricWidget"));
        }
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(TINY_PNG);
        let inner = format!("<MetricWidgetImage>{b64}</MetricWidgetImage>");
        Ok(xml_response(
            "GetMetricWidgetImage",
            &inner,
            &req.request_id,
        ))
    }
}
