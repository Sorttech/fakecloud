//! Resource metrics configurations: per-resource detailed-metric collection
//! settings (Create/Get/Update/Delete), keyed by resource ARN per region.
//!
//! Each resource has at most one configuration. `MetricSelections` holds at
//! most one selection whose `IncludeMetrics` names the metrics to collect;
//! omitting it collects every available detailed metric.

use std::collections::BTreeMap;

use chrono::Utc;
use fakecloud_core::query::optional_query_param;
use fakecloud_core::service::{AwsRequest, AwsResponse, AwsServiceError};

use crate::otel::fmt_ts;
use crate::service::{
    collect_indexed, conflict, empty_metadata_response, missing_param, not_found, validation_error,
    xml_escape, xml_response, CloudWatchService,
};
use crate::state::ResourceMetricsConfiguration;

/// `ResourceArn` is 20-2048 chars matching
/// `^arn:[a-zA-Z0-9-]+:[a-zA-Z0-9-]+:[a-zA-Z0-9-]*:\d{12}:.+$`.
fn validate_resource_arn(arn: &str) -> Result<(), AwsServiceError> {
    let len = arn.chars().count();
    let invalid = || validation_error(format!("ResourceArn '{arn}' is not a valid ARN"));
    if !(20..=2048).contains(&len) {
        return Err(invalid());
    }
    let parts: Vec<&str> = arn.splitn(6, ':').collect();
    if parts.len() != 6 || parts[0] != "arn" {
        return Err(invalid());
    }
    let seg_ok = |s: &str| s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
    let ok = !parts[1].is_empty()
        && seg_ok(parts[1])
        && !parts[2].is_empty()
        && seg_ok(parts[2])
        && seg_ok(parts[3])
        && parts[4].len() == 12
        && parts[4].chars().all(|c| c.is_ascii_digit())
        && !parts[5].is_empty();
    if ok {
        Ok(())
    } else {
        Err(invalid())
    }
}

fn resource_arn(req: &AwsRequest) -> Result<String, AwsServiceError> {
    let arn =
        optional_query_param(req, "ResourceArn").ok_or_else(|| missing_param("ResourceArn"))?;
    validate_resource_arn(&arn)?;
    Ok(arn)
}

/// Parse `MetricSelections` (0 or 1 selection). Returns `None` when omitted.
fn parse_metric_selections(req: &AwsRequest) -> Result<Option<Vec<String>>, AwsServiceError> {
    let selections = collect_indexed(req, "MetricSelections");
    if selections.is_empty() {
        return Ok(None);
    }
    if selections.len() > 1 {
        return Err(validation_error(
            "MetricSelections must contain exactly one selection",
        ));
    }
    let mut indexed: BTreeMap<u32, String> = BTreeMap::new();
    for (k, v) in &selections[0] {
        if let Some(idx) = k.strip_prefix("IncludeMetrics.member.") {
            if let Ok(i) = idx.parse::<u32>() {
                indexed.insert(i, v.clone());
            }
        }
    }
    let names: Vec<String> = indexed.into_values().collect();
    if names.is_empty() || names.len() > 500 {
        return Err(validation_error(
            "MetricSelections.IncludeMetrics must contain between 1 and 500 metric names",
        ));
    }
    if let Some(bad) = names
        .iter()
        .find(|n| !(1..=255).contains(&n.chars().count()))
    {
        return Err(validation_error(format!(
            "Metric name '{bad}' must be 1-255 characters"
        )));
    }
    Ok(Some(names))
}

fn render(cfg: &ResourceMetricsConfiguration) -> String {
    let mut s = String::from("<ResourceMetricsConfiguration>");
    s.push_str(&format!(
        "<ResourceArn>{}</ResourceArn>",
        xml_escape(&cfg.resource_arn)
    ));
    s.push_str(&format!(
        "<CreatedAt>{}</CreatedAt>",
        fmt_ts(cfg.created_at)
    ));
    s.push_str(&format!(
        "<UpdatedAt>{}</UpdatedAt>",
        fmt_ts(cfg.updated_at)
    ));
    if let Some(names) = &cfg.include_metrics {
        s.push_str("<MetricSelections><member><IncludeMetrics>");
        for n in names {
            s.push_str(&format!("<member>{}</member>", xml_escape(n)));
        }
        s.push_str("</IncludeMetrics></member></MetricSelections>");
    }
    s.push_str("</ResourceMetricsConfiguration>");
    s
}

fn missing_config(arn: &str) -> AwsServiceError {
    not_found(format!(
        "No resource metrics configuration exists for resource {arn}"
    ))
}

impl CloudWatchService {
    pub(crate) fn create_resource_metrics_configuration(
        &self,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        let arn = resource_arn(req)?;
        let include_metrics = parse_metric_selections(req)?;
        let mut state = self.state.write();
        let configs = state
            .get_or_create(&req.account_id)
            .resource_metrics_in_mut(&req.region);
        if configs.contains_key(&arn) {
            return Err(conflict(format!(
                "A resource metrics configuration already exists for resource {arn}"
            )));
        }
        let now = Utc::now();
        let cfg = ResourceMetricsConfiguration {
            resource_arn: arn.clone(),
            include_metrics,
            created_at: now,
            updated_at: now,
        };
        let inner = render(&cfg);
        configs.insert(arn, cfg);
        Ok(xml_response(
            "CreateResourceMetricsConfiguration",
            &inner,
            &req.request_id,
        ))
    }

    pub(crate) fn get_resource_metrics_configuration(
        &self,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        let arn = resource_arn(req)?;
        let state = self.state.read();
        let cfg = state
            .get(&req.account_id)
            .and_then(|a| a.resource_metrics_in(&req.region))
            .and_then(|m| m.get(&arn))
            .ok_or_else(|| missing_config(&arn))?;
        Ok(xml_response(
            "GetResourceMetricsConfiguration",
            &render(cfg),
            &req.request_id,
        ))
    }

    pub(crate) fn update_resource_metrics_configuration(
        &self,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        let arn = resource_arn(req)?;
        let include_metrics = parse_metric_selections(req)?;
        let mut state = self.state.write();
        let cfg = state
            .get_or_create(&req.account_id)
            .resource_metrics_in_mut(&req.region)
            .get_mut(&arn)
            .ok_or_else(|| missing_config(&arn))?;
        // Selections replace (never merge); omitting them collects everything.
        cfg.include_metrics = include_metrics;
        cfg.updated_at = Utc::now();
        Ok(xml_response(
            "UpdateResourceMetricsConfiguration",
            &render(cfg),
            &req.request_id,
        ))
    }

    pub(crate) fn delete_resource_metrics_configuration(
        &self,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        let arn = resource_arn(req)?;
        let mut state = self.state.write();
        state
            .get_or_create(&req.account_id)
            .resource_metrics_in_mut(&req.region)
            .remove(&arn)
            .ok_or_else(|| missing_config(&arn))?;
        Ok(empty_metadata_response(
            "DeleteResourceMetricsConfiguration",
            &req.request_id,
        ))
    }
}
