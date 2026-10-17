//! Product-attribute rules for Cost Explorer requests.
//!
//! Product attributes (`Expression.ProductAttributes`, the `PRODUCT_ATTRIBUTE`
//! group-by type, and `GetDimensionValues` with `Dimension=PRODUCT_ATTRIBUTE`
//! plus `DimensionKey`) break the costs of supported services down by
//! service-specific attributes. Amazon Bedrock is the only supported service,
//! with the keys `provider`, `model`, `inferenceType` and `feature`.
//!
//! This module enforces the request contract the model documents:
//!
//! * Only `GetCostAndUsage`, `GetCostAndUsageWithResources`,
//!   `GetDimensionValues` (in the `COST_AND_USAGE` context), `GetTags` and
//!   `GetCostCategories` accept a `ProductAttributes` filter; only the two
//!   cost-and-usage operations accept a `PRODUCT_ATTRIBUTE` group.
//! * `ProductAttributeValues`: `Key` is required and matches
//!   `^([a-zA-Z][a-zA-Z0-9]*)?$`; `MatchOptions` are `EQUALS`, `ABSENT` and
//!   `CASE_SENSITIVE` only; `ABSENT` omits `Values`, otherwise `Values` is
//!   required.
//! * `DimensionKey` only with `Dimension=PRODUCT_ATTRIBUTE`; `SortBy` with
//!   `PRODUCT_ATTRIBUTE` requires a `DimensionKey`.
//! * SERVICE filter rules: whenever product attributes are in play a SERVICE
//!   filter must apply to the whole request (combined with `And`, present in
//!   every `Or` branch, never under `Not`) and name only supported services;
//!   `GetDimensionValues`, `GetTags` and `GetCostCategories` require one when
//!   the filter includes `ProductAttributes`.
//! * Product-attribute data exists only for periods starting on or after
//!   2026-09-01; earlier periods fail with `DataUnavailableException`.
//!
//! Every violation is a `ValidationException`, Cost Explorer's common
//! input-validation error.

use std::collections::BTreeSet;

use http::StatusCode;
use serde_json::Value;

use fakecloud_core::service::AwsServiceError;

/// First day for which Cost Explorer has product-attribute data.
const DATA_START: &str = "2026-09-01";

/// The Cost Explorer `SERVICE` dimension value of the one supported service.
const BEDROCK_SERVICE: &str = "Amazon Bedrock";

/// Bedrock costs for third-party models sold through AWS Marketplace appear
/// under per-model service names carrying this suffix (for example
/// `Claude 3.5 Sonnet (Amazon Bedrock Edition)`); they are costs of the same
/// supported service.
const BEDROCK_EDITION_SUFFIX: &str = "(Amazon Bedrock Edition)";

const KEY_MAX_LEN: usize = 1024;
const VALUES_MAX: usize = 6000;
const VALUE_MAX_LEN: usize = 1024;

/// How an operation handles product attributes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Support {
    /// `GetCostAndUsage` / `GetCostAndUsageWithResources`: filter and group;
    /// SERVICE filter optional.
    CostAndUsage,
    /// `GetDimensionValues`: filter (COST_AND_USAGE context) and the
    /// `PRODUCT_ATTRIBUTE` dimension; SERVICE filter required with a
    /// `ProductAttributes` filter.
    DimensionValues,
    /// `GetTags` / `GetCostCategories`: filter only; SERVICE filter required
    /// with a `ProductAttributes` filter.
    FilterOnly,
    /// Every other operation: product attributes are not accepted.
    Unsupported,
}

fn support_for(action: &str) -> Support {
    match action {
        "GetCostAndUsage" | "GetCostAndUsageWithResources" => Support::CostAndUsage,
        "GetDimensionValues" => Support::DimensionValues,
        "GetTags" | "GetCostCategories" => Support::FilterOnly,
        _ => Support::Unsupported,
    }
}

fn validation(msg: &str) -> AwsServiceError {
    AwsServiceError::aws_error(StatusCode::BAD_REQUEST, "ValidationException", msg)
}

fn data_unavailable() -> AwsServiceError {
    AwsServiceError::aws_error(
        StatusCode::BAD_REQUEST,
        "DataUnavailableException",
        format!(
            "Product attribute data is available only for time periods that start on or \
             after {DATA_START}."
        ),
    )
}

/// Whether a `SERVICE` dimension value names a supported service.
pub(crate) fn is_supported_service(name: &str) -> bool {
    name == BEDROCK_SERVICE || name.ends_with(BEDROCK_EDITION_SUFFIX)
}

/// Apply the product-attribute rules to a request. Called after the flat
/// model-constraint pass, before the handler runs.
pub(crate) fn check_request(action: &str, body: &Value) -> Result<(), AwsServiceError> {
    let support = support_for(action);

    if support == Support::Unsupported {
        for (member, expr) in stored_expressions(action, body) {
            if expression_has_product_attributes(expr) {
                return Err(validation(&format!(
                    "ProductAttributes isn't supported in {member} for {action}."
                )));
            }
        }
        if has_product_attribute_group(body) {
            return Err(validation(&format!(
                "Group by PRODUCT_ATTRIBUTE isn't supported for {action}."
            )));
        }
        return Ok(());
    }

    let filter = body.get("Filter").filter(|v| v.is_object());
    if let Some(f) = filter {
        validate_expression(f)?;
    }
    let pa_filter = filter.is_some_and(expression_has_product_attributes);

    let pa_group = has_product_attribute_group(body);
    if pa_group {
        if support != Support::CostAndUsage {
            return Err(validation(&format!(
                "Group by PRODUCT_ATTRIBUTE isn't supported for {action}."
            )));
        }
        validate_product_attribute_groups(body)?;
    }

    let mut pa_dimension = false;
    if support == Support::DimensionValues {
        pa_dimension = check_dimension_values(body, pa_filter)?;
    }

    if !(pa_filter || pa_group || pa_dimension) {
        return Ok(());
    }

    let required = pa_filter && matches!(support, Support::DimensionValues | Support::FilterOnly);
    check_service_filter(filter, required)?;

    if period_predates_data(body) {
        return Err(data_unavailable());
    }
    Ok(())
}

/// `GetDimensionValues`-specific rules. Returns whether the request lists the
/// `PRODUCT_ATTRIBUTE` dimension.
fn check_dimension_values(body: &Value, pa_filter: bool) -> Result<bool, AwsServiceError> {
    let dimension = body.get("Dimension").and_then(Value::as_str);
    let is_pa = dimension == Some("PRODUCT_ATTRIBUTE");
    let context = body
        .get("Context")
        .and_then(Value::as_str)
        .unwrap_or("COST_AND_USAGE");
    let dimension_key = body.get("DimensionKey").filter(|v| !v.is_null());
    // An empty DimensionKey is the same as omitting it.
    let key_set = dimension_key
        .and_then(Value::as_str)
        .is_some_and(|k| !k.is_empty());

    if key_set && !is_pa {
        return Err(validation(
            "DimensionKey can be specified only when Dimension is PRODUCT_ATTRIBUTE.",
        ));
    }
    if is_pa && context != "COST_AND_USAGE" {
        return Err(validation(
            "Dimension PRODUCT_ATTRIBUTE is supported only in the COST_AND_USAGE context.",
        ));
    }
    if pa_filter && context != "COST_AND_USAGE" {
        return Err(validation(
            "ProductAttributes filters are supported only in the COST_AND_USAGE context.",
        ));
    }
    let sort_by = body
        .get("SortBy")
        .and_then(Value::as_array)
        .is_some_and(|a| !a.is_empty());
    if is_pa && sort_by && !key_set {
        return Err(validation(
            "DimensionKey is required when SortBy is specified for Dimension PRODUCT_ATTRIBUTE.",
        ));
    }
    Ok(is_pa)
}

/// The `Expression`-typed members an operation that does not support product
/// attributes carries, labelled by member name.
fn stored_expressions<'a>(action: &str, body: &'a Value) -> Vec<(&'static str, &'a Value)> {
    let mut out = Vec::new();
    match action {
        "CreateAnomalyMonitor" => {
            if let Some(e) = body.pointer("/AnomalyMonitor/MonitorSpecification") {
                out.push(("MonitorSpecification", e));
            }
        }
        "CreateAnomalySubscription" => {
            if let Some(e) = body.pointer("/AnomalySubscription/ThresholdExpression") {
                out.push(("ThresholdExpression", e));
            }
        }
        "UpdateAnomalySubscription" => {
            if let Some(e) = body.get("ThresholdExpression") {
                out.push(("ThresholdExpression", e));
            }
        }
        "CreateCostCategoryDefinition" | "UpdateCostCategoryDefinition" => {
            for rule in body
                .get("Rules")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let Some(e) = rule.get("Rule") {
                    out.push(("Rules", e));
                }
            }
        }
        _ => {
            if let Some(e) = body.get("Filter") {
                out.push(("Filter", e));
            }
        }
    }
    out
}

/// Child expressions of `And`/`Or` (lists) and `Not` (single).
fn children(expr: &Value) -> impl Iterator<Item = &Value> {
    let lists = ["And", "Or"]
        .into_iter()
        .filter_map(move |op| expr.get(op).and_then(Value::as_array))
        .flatten();
    lists.chain(expr.get("Not").filter(|v| v.is_object()))
}

fn present(expr: &Value, member: &str) -> bool {
    expr.get(member).is_some_and(|v| !v.is_null())
}

/// Whether `ProductAttributes` appears anywhere in the expression tree.
pub(crate) fn expression_has_product_attributes(expr: &Value) -> bool {
    present(expr, "ProductAttributes") || children(expr).any(expression_has_product_attributes)
}

/// Validate every `ProductAttributeValues` leaf in the expression tree.
fn validate_expression(expr: &Value) -> Result<(), AwsServiceError> {
    if let Some(pa) = expr.get("ProductAttributes").filter(|v| !v.is_null()) {
        validate_product_attribute_values(pa)?;
    }
    for child in children(expr) {
        validate_expression(child)?;
    }
    Ok(())
}

/// `^([a-zA-Z][a-zA-Z0-9]*)?$`, at most 1024 characters.
fn valid_attribute_key(key: &str) -> bool {
    if key.chars().count() > KEY_MAX_LEN {
        return false;
    }
    let mut chars = key.chars();
    match chars.next() {
        None => true,
        Some(c) if c.is_ascii_alphabetic() => chars.all(|c| c.is_ascii_alphanumeric()),
        Some(_) => false,
    }
}

fn validate_product_attribute_values(pa: &Value) -> Result<(), AwsServiceError> {
    let key = pa
        .get("Key")
        .and_then(Value::as_str)
        .ok_or_else(|| validation("ProductAttributes.Key is a required field."))?;
    if !valid_attribute_key(key) {
        return Err(validation(&format!(
            "ProductAttributes.Key '{key}' must match the pattern ^([a-zA-Z][a-zA-Z0-9]*)?$ \
             and have length less than or equal to {KEY_MAX_LEN}."
        )));
    }

    let options: Vec<&str> = pa
        .get("MatchOptions")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    if let Some(bad) = options
        .iter()
        .find(|o| !matches!(**o, "EQUALS" | "ABSENT" | "CASE_SENSITIVE"))
    {
        return Err(validation(&format!(
            "MatchOptions '{bad}' is not supported for ProductAttributes. Valid values are: \
             EQUALS, ABSENT, CASE_SENSITIVE."
        )));
    }
    let absent = options.contains(&"ABSENT");
    if absent && options.iter().any(|o| *o != "ABSENT") {
        return Err(validation(
            "MatchOptions ABSENT can't be combined with other match options for \
             ProductAttributes.",
        ));
    }

    let values = pa.get("Values").filter(|v| !v.is_null());
    match (absent, values) {
        (true, Some(_)) => Err(validation(
            "Values must be omitted when MatchOptions is ABSENT for ProductAttributes.",
        )),
        (true, None) => Ok(()),
        (false, None) => Err(validation(
            "ProductAttributes.Values is required unless MatchOptions is ABSENT.",
        )),
        (false, Some(v)) => {
            let list = v
                .as_array()
                .ok_or_else(|| validation("ProductAttributes.Values must be a list."))?;
            if list.is_empty() || list.len() > VALUES_MAX {
                return Err(validation(&format!(
                    "ProductAttributes.Values must contain between 1 and {VALUES_MAX} values."
                )));
            }
            for item in list {
                let s = item
                    .as_str()
                    .ok_or_else(|| validation("ProductAttributes.Values must be strings."))?;
                if s.chars().count() > VALUE_MAX_LEN {
                    return Err(validation(&format!(
                        "ProductAttributes.Values members must have length less than or equal \
                         to {VALUE_MAX_LEN}."
                    )));
                }
            }
            Ok(())
        }
    }
}

fn product_attribute_groups(body: &Value) -> impl Iterator<Item = &Value> {
    body.get("GroupBy")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|g| g.get("Type").and_then(Value::as_str) == Some("PRODUCT_ATTRIBUTE"))
}

fn has_product_attribute_group(body: &Value) -> bool {
    product_attribute_groups(body).next().is_some()
}

/// A `PRODUCT_ATTRIBUTE` group names a product attribute key.
fn validate_product_attribute_groups(body: &Value) -> Result<(), AwsServiceError> {
    for group in product_attribute_groups(body) {
        let key = group.get("Key").and_then(Value::as_str).unwrap_or("");
        if key.is_empty() {
            return Err(validation(
                "GroupBy Key is required when Type is PRODUCT_ATTRIBUTE.",
            ));
        }
        if !valid_attribute_key(key) {
            return Err(validation(&format!(
                "GroupBy Key '{key}' isn't a valid product attribute key. Keys must match \
                 ^([a-zA-Z][a-zA-Z0-9]*)?$."
            )));
        }
    }
    Ok(())
}

fn service_dimension(expr: &Value) -> Option<&Value> {
    expr.get("Dimensions")
        .filter(|d| d.get("Key").and_then(Value::as_str) == Some("SERVICE"))
}

fn dimension_values(dim: &Value) -> BTreeSet<String> {
    dim.get("Values")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// The set of services an expression restricts the whole request to, or
/// `None` when it does not. A SERVICE leaf restricts; `And` restricts when any
/// operand does (intersection); `Or` restricts only when every branch does
/// (union); `Not` never restricts.
fn service_scope(expr: &Value) -> Option<BTreeSet<String>> {
    if let Some(dim) = service_dimension(expr) {
        return Some(dimension_values(dim));
    }
    if let Some(and) = expr.get("And").and_then(Value::as_array) {
        return and
            .iter()
            .filter_map(service_scope)
            .reduce(|a, b| a.intersection(&b).cloned().collect());
    }
    if let Some(or) = expr.get("Or").and_then(Value::as_array) {
        if or.is_empty() {
            return None;
        }
        let mut union = BTreeSet::new();
        for branch in or {
            union.extend(service_scope(branch)?);
        }
        return Some(union);
    }
    None
}

/// Every SERVICE dimension value named anywhere in the expression tree, and
/// whether any SERVICE leaf exists at all.
fn collect_service_values(expr: &Value, out: &mut Vec<String>, found: &mut bool) {
    if let Some(dim) = service_dimension(expr) {
        *found = true;
        out.extend(dimension_values(dim));
    }
    for child in children(expr) {
        collect_service_values(child, out, found);
    }
}

fn check_service_filter(filter: Option<&Value>, required: bool) -> Result<(), AwsServiceError> {
    let mut names = Vec::new();
    let mut found = false;
    if let Some(f) = filter {
        collect_service_values(f, &mut names, &mut found);
    }
    if !found {
        if required {
            return Err(validation(
                "A SERVICE filter is required when the filter includes ProductAttributes.",
            ));
        }
        return Ok(());
    }
    if filter.and_then(service_scope).is_none() {
        return Err(validation(
            "The SERVICE filter must apply to the whole request: combine it with other \
             filters by using And, include it in every branch of an Or, and don't use it \
             inside Not.",
        ));
    }
    if let Some(bad) = names.iter().find(|n| !is_supported_service(n)) {
        return Err(validation(&format!(
            "The SERVICE filter can contain only services that support product attributes. \
             '{bad}' isn't supported."
        )));
    }
    Ok(())
}

/// Whether the requested `TimePeriod` starts before product-attribute data
/// exists. Unparseable starts are left to the handler.
fn period_predates_data(body: &Value) -> bool {
    let fmt = "%Y-%m-%d";
    let Some(start) = body
        .pointer("/TimePeriod/Start")
        .and_then(Value::as_str)
        .and_then(|s| s.get(0..10))
        .and_then(|s| chrono::NaiveDate::parse_from_str(s, fmt).ok())
    else {
        return false;
    };
    chrono::NaiveDate::parse_from_str(DATA_START, fmt).is_ok_and(|cutoff| start < cutoff)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn code(action: &str, body: Value) -> Option<String> {
        check_request(action, &body)
            .err()
            .map(|e| e.code().to_string())
    }

    fn pa(key: &str, values: &[&str]) -> Value {
        json!({ "ProductAttributes": { "Key": key, "Values": values } })
    }

    fn service(names: &[&str]) -> Value {
        json!({ "Dimensions": { "Key": "SERVICE", "Values": names } })
    }

    const PERIOD: &str = r#"{"Start":"2026-09-01","End":"2026-10-01"}"#;

    fn period() -> Value {
        serde_json::from_str(PERIOD).unwrap()
    }

    #[test]
    fn supported_service_names() {
        assert!(is_supported_service("Amazon Bedrock"));
        assert!(is_supported_service(
            "Claude 3.5 Sonnet (Amazon Bedrock Edition)"
        ));
        assert!(!is_supported_service(
            "Amazon Elastic Compute Cloud - Compute"
        ));
        assert!(!is_supported_service("amazon bedrock"));
    }

    #[test]
    fn cost_and_usage_accepts_product_attribute_filter_without_service() {
        let body = json!({ "TimePeriod": period(), "Filter": pa("model", &["Claude Sonnet 5"]) });
        assert_eq!(code("GetCostAndUsage", body), None);
    }

    #[test]
    fn nested_product_attributes_are_validated() {
        let bad = json!({ "ProductAttributes": { "Key": "1model", "Values": ["x"] } });
        let body = json!({
            "TimePeriod": period(),
            "Filter": { "And": [ pa("provider", &["Anthropic"]), { "Not": { "Or": [bad] } } ] }
        });
        assert_eq!(
            code("GetCostAndUsage", body).as_deref(),
            Some("ValidationException")
        );
    }

    #[test]
    fn product_attribute_key_is_required() {
        let body = json!({ "TimePeriod": period(), "Filter": { "ProductAttributes": { "Values": ["x"] } } });
        assert_eq!(
            code("GetCostAndUsage", body).as_deref(),
            Some("ValidationException")
        );
    }

    #[test]
    fn match_options_rules() {
        let with = |opts: Value, values: Option<Value>| {
            let mut leaf = json!({ "Key": "model", "MatchOptions": opts });
            if let Some(v) = values {
                leaf["Values"] = v;
            }
            code(
                "GetCostAndUsage",
                json!({ "TimePeriod": period(), "Filter": { "ProductAttributes": leaf } }),
            )
        };
        // ABSENT without Values matches costs lacking the key.
        assert_eq!(with(json!(["ABSENT"]), None), None);
        // EQUALS + CASE_SENSITIVE with Values.
        assert_eq!(
            with(json!(["EQUALS", "CASE_SENSITIVE"]), Some(json!(["x"]))),
            None
        );
        // ABSENT with Values, EQUALS without Values, and options Cost
        // Explorer doesn't support for product attributes are rejected.
        for (opts, values) in [
            (json!(["ABSENT"]), Some(json!(["x"]))),
            (json!(["EQUALS"]), None),
            (json!(["ABSENT", "CASE_SENSITIVE"]), None),
            (json!(["STARTS_WITH"]), Some(json!(["x"]))),
            (json!(["CASE_INSENSITIVE"]), Some(json!(["x"]))),
            (json!(["EQUALS"]), Some(json!([]))),
        ] {
            assert_eq!(
                with(opts.clone(), values.clone()).as_deref(),
                Some("ValidationException"),
                "{opts} {values:?}"
            );
        }
    }

    #[test]
    fn service_filter_must_apply_to_whole_request() {
        let ok = json!({ "And": [ service(&["Amazon Bedrock"]), pa("model", &["m"]) ] });
        let ok_or = json!({ "Or": [
            { "And": [ service(&["Amazon Bedrock"]), pa("model", &["a"]) ] },
            { "And": [ service(&["Amazon Bedrock"]), pa("model", &["b"]) ] },
        ] });
        let partial_or = json!({ "Or": [ service(&["Amazon Bedrock"]), pa("model", &["m"]) ] });
        let under_not =
            json!({ "And": [ { "Not": service(&["Amazon Bedrock"]) }, pa("model", &["m"]) ] });
        for (f, want) in [
            (ok, None),
            (ok_or, None),
            (partial_or, Some("ValidationException")),
            (under_not, Some("ValidationException")),
        ] {
            assert_eq!(
                code(
                    "GetCostAndUsage",
                    json!({ "TimePeriod": period(), "Filter": f })
                )
                .as_deref(),
                want
            );
        }
    }

    #[test]
    fn service_filter_must_name_only_supported_services() {
        let f = json!({ "And": [ service(&["Amazon Bedrock", "Amazon Simple Storage Service"]), pa("model", &["m"]) ] });
        assert_eq!(
            code(
                "GetCostAndUsage",
                json!({ "TimePeriod": period(), "Filter": f })
            )
            .as_deref(),
            Some("ValidationException")
        );
    }

    #[test]
    fn service_filter_required_for_tags_cost_categories_dimension_values() {
        for action in ["GetTags", "GetCostCategories", "GetDimensionValues"] {
            let mut body = json!({ "TimePeriod": period(), "Filter": pa("model", &["m"]) });
            if action == "GetDimensionValues" {
                body["Dimension"] = json!("REGION");
            }
            assert_eq!(
                code(action, body.clone()).as_deref(),
                Some("ValidationException"),
                "{action}"
            );
            body["Filter"] =
                json!({ "And": [ service(&["Amazon Bedrock"]), pa("model", &["m"]) ] });
            assert_eq!(code(action, body), None, "{action}");
        }
    }

    #[test]
    fn unsupported_operations_reject_product_attributes() {
        let f = pa("model", &["m"]);
        for (action, body) in [
            ("GetCostForecast", json!({ "Filter": f.clone() })),
            ("GetReservationCoverage", json!({ "Filter": f.clone() })),
            (
                "UpdateAnomalySubscription",
                json!({ "ThresholdExpression": f.clone() }),
            ),
            (
                "CreateCostCategoryDefinition",
                json!({ "Rules": [ { "Value": "v", "Rule": f.clone() } ] }),
            ),
            (
                "GetCostAndUsageComparisons",
                json!({ "GroupBy": [ { "Type": "PRODUCT_ATTRIBUTE", "Key": "model" } ] }),
            ),
        ] {
            assert_eq!(
                code(action, body).as_deref(),
                Some("ValidationException"),
                "{action}"
            );
        }
    }

    #[test]
    fn product_attribute_group_rules() {
        let group = |key: Value| json!({ "TimePeriod": period(), "GroupBy": [ { "Type": "PRODUCT_ATTRIBUTE", "Key": key } ] });
        assert_eq!(code("GetCostAndUsage", group(json!("model"))), None);
        assert_eq!(
            code("GetCostAndUsageWithResources", group(json!("model"))),
            None
        );
        // A key that doesn't exist (wrong case) is not an error.
        assert_eq!(code("GetCostAndUsage", group(json!("Model"))), None);
        assert_eq!(
            code("GetCostAndUsage", group(json!(""))).as_deref(),
            Some("ValidationException")
        );
        assert_eq!(
            code("GetCostAndUsage", group(json!("model$x"))).as_deref(),
            Some("ValidationException")
        );
        assert_eq!(
            code("GetTags", group(json!("model"))).as_deref(),
            Some("ValidationException")
        );
    }

    #[test]
    fn dimension_key_rules() {
        let dv = |extra: Value| {
            let mut b = json!({ "TimePeriod": period(), "Dimension": "PRODUCT_ATTRIBUTE" });
            for (k, v) in extra.as_object().unwrap() {
                b[k] = v.clone();
            }
            code("GetDimensionValues", b)
        };
        assert_eq!(dv(json!({})), None);
        assert_eq!(dv(json!({ "DimensionKey": "" })), None);
        assert_eq!(dv(json!({ "DimensionKey": "model" })), None);
        assert_eq!(
            dv(json!({ "Dimension": "REGION", "DimensionKey": "model" })).as_deref(),
            Some("ValidationException")
        );
        // An empty key counts as omitted, so any dimension accepts it.
        assert_eq!(
            dv(json!({ "Dimension": "REGION", "DimensionKey": "" })),
            None
        );
        assert_eq!(
            dv(json!({ "Context": "RESERVATIONS" })).as_deref(),
            Some("ValidationException")
        );
        let sort = json!([{ "Key": "UnblendedCost", "SortOrder": "DESCENDING" }]);
        assert_eq!(
            dv(json!({ "SortBy": sort.clone() })).as_deref(),
            Some("ValidationException")
        );
        assert_eq!(
            dv(json!({ "SortBy": sort.clone(), "DimensionKey": "" })).as_deref(),
            Some("ValidationException")
        );
        assert_eq!(dv(json!({ "SortBy": sort, "DimensionKey": "model" })), None);
        // The PRODUCT_ATTRIBUTE dimension does not by itself require a SERVICE
        // filter, but one that is present must name only supported services.
        assert_eq!(dv(json!({ "Filter": service(&["Amazon Bedrock"]) })), None);
        assert_eq!(
            dv(json!({ "Filter": service(&["AWS Lambda"]) })).as_deref(),
            Some("ValidationException")
        );
    }

    #[test]
    fn product_attribute_filter_needs_cost_and_usage_context() {
        let body = json!({
            "TimePeriod": period(),
            "Dimension": "REGION",
            "Context": "SAVINGS_PLANS",
            "Filter": { "And": [ service(&["Amazon Bedrock"]), pa("model", &["m"]) ] },
        });
        assert_eq!(
            code("GetDimensionValues", body).as_deref(),
            Some("ValidationException")
        );
    }

    #[test]
    fn periods_before_data_start_are_unavailable() {
        let early = json!({ "Start": "2026-08-31", "End": "2026-10-01" });
        let hourly_early =
            json!({ "Start": "2026-08-31T23:00:00Z", "End": "2026-09-02T00:00:00Z" });
        for tp in [early, hourly_early] {
            assert_eq!(
                code(
                    "GetCostAndUsage",
                    json!({ "TimePeriod": tp.clone(), "Filter": pa("model", &["m"]) })
                )
                .as_deref(),
                Some("DataUnavailableException")
            );
            assert_eq!(
                code("GetCostAndUsage", json!({ "TimePeriod": tp.clone(), "GroupBy": [ { "Type": "PRODUCT_ATTRIBUTE", "Key": "model" } ] })).as_deref(),
                Some("DataUnavailableException")
            );
            assert_eq!(
                code(
                    "GetDimensionValues",
                    json!({ "TimePeriod": tp.clone(), "Dimension": "PRODUCT_ATTRIBUTE" })
                )
                .as_deref(),
                Some("DataUnavailableException")
            );
            // Without product attributes the period is fine.
            assert_eq!(
                code(
                    "GetCostAndUsage",
                    json!({ "TimePeriod": tp, "Filter": service(&["AWS Lambda"]) })
                ),
                None
            );
        }
        // Validation runs before the data-availability check.
        let tp = json!({ "Start": "2025-01-01", "End": "2025-02-01" });
        assert_eq!(
            code(
                "GetTags",
                json!({ "TimePeriod": tp, "Filter": pa("model", &["m"]) })
            )
            .as_deref(),
            Some("ValidationException")
        );
    }
}
