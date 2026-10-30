//! Integration request parameter mapping (`integration.request.*`).
//!
//! An integration's `requestParameters` map a source expression to a destination in the request
//! sent to the backend. The destinations are `integration.request.path.<name>` (substituted into
//! `{<name>}` in the integration URI), `integration.request.querystring.<name>` and
//! `integration.request.header.<name>`. The sources are the method request's path, query string
//! (single and multi-value) and headers, `stageVariables.<name>`, `context.<name>` and a
//! `'static value'` in single quotes
//! (<https://docs.aws.amazon.com/apigateway/latest/developerguide/rest-api-parameter-mapping-sources.md>,
//! fetched 2026-10-21).
//!
//! Not mapped: the request body sources (`method.request.body` and its JSONPath form). A mapping
//! whose source cannot be resolved is left out of the backend request.
//!
//! Applies to proxy and non-proxy integrations alike
//! (<https://docs.aws.amazon.com/apigateway/latest/developerguide/rest-api-parameter-mapping.md>).

use super::*;
use percent_encoding::{percent_decode_str, utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};

/// Everything but the RFC 3986 unreserved characters, and `/` so a greedy `{proxy+}` value keeps
/// its path separators.
const PATH_VALUE: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~')
    .remove(b'/');
const QUERY_PART: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// The parts of the backend request that `requestParameters` set.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct MappedRequest {
    /// `integration.request.path.<name>` -> value, for `{<name>}` in the URI.
    path: BTreeMap<String, String>,
    /// `integration.request.querystring.<name>`, in the order the mapping lists them.
    query: Vec<(String, String)>,
    /// `integration.request.header.<name>`.
    headers: Vec<(String, String)>,
}

impl MappedRequest {
    pub(super) fn build(
        integration: &Integration,
        req: &AwsRequest,
        path_params: &BTreeMap<String, String>,
        stage_vars: &BTreeMap<String, String>,
        context: &Value,
    ) -> Self {
        let mut mapped = Self::default();
        for (destination, source) in &integration.request_parameters {
            let Some(value) = resolve_source(source, req, path_params, stage_vars, context) else {
                continue;
            };
            if let Some(name) = destination.strip_prefix("integration.request.path.") {
                mapped.path.insert(name.to_string(), value);
            } else if let Some(name) = destination.strip_prefix("integration.request.querystring.")
            {
                mapped.query.push((name.to_string(), value));
            } else if let Some(name) = destination.strip_prefix("integration.request.header.") {
                mapped.headers.push((name.to_string(), value));
            }
        }
        mapped
    }

    /// The same mapping with the path parameters dropped, for a URL they have already been put in.
    pub(super) fn without_path(other: &Self) -> Self {
        Self {
            path: BTreeMap::new(),
            query: other.query.clone(),
            headers: other.headers.clone(),
        }
    }

    pub(super) fn headers(&self) -> &[(String, String)] {
        &self.headers
    }

    /// The URL to call: `uri` with its `{name}` / `{name+}` placeholders filled from the mapped
    /// path parameters, then the query string. `forward_query` is the client's own query string,
    /// which a proxy integration passes through; a mapped parameter replaces a client one of the
    /// same name.
    pub(super) fn url_for(&self, uri: &str, forward_query: Option<&str>) -> String {
        let mut url = uri.to_string();
        for (name, value) in &self.path {
            let encoded = utf8_percent_encode(value, PATH_VALUE).to_string();
            url = url
                .replace(&format!("{{{name}+}}"), &encoded)
                .replace(&format!("{{{name}}}"), &encoded);
        }
        let mut parts: Vec<String> = forward_query
            .unwrap_or("")
            .split('&')
            .filter(|p| !p.is_empty())
            .filter(|p| {
                let key = p.split('=').next().unwrap_or("");
                let key = percent_decode_str(key).decode_utf8_lossy();
                !self.query.iter().any(|(name, _)| *name == key)
            })
            .map(str::to_string)
            .collect();
        for (name, value) in &self.query {
            parts.push(format!(
                "{}={}",
                utf8_percent_encode(name, QUERY_PART),
                utf8_percent_encode(value, QUERY_PART)
            ));
        }
        if parts.is_empty() {
            return url;
        }
        let separator = if url.contains('?') { '&' } else { '?' };
        format!("{url}{separator}{}", parts.join("&"))
    }
}

fn resolve_source(
    source: &str,
    req: &AwsRequest,
    path_params: &BTreeMap<String, String>,
    stage_vars: &BTreeMap<String, String>,
    context: &Value,
) -> Option<String> {
    if let Some(literal) = source
        .strip_prefix('\'')
        .and_then(|rest| rest.strip_suffix('\''))
    {
        return Some(literal.to_string());
    }
    if let Some(name) = source.strip_prefix("method.request.path.") {
        return path_params.get(name).cloned();
    }
    if let Some(name) = source.strip_prefix("method.request.querystring.") {
        return req.query_params.get(name).cloned();
    }
    if let Some(name) = source.strip_prefix("method.request.multivaluequerystring.") {
        let values: Vec<String> = req
            .raw_query
            .split('&')
            .filter_map(|pair| {
                let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
                (percent_decode_str(key).decode_utf8_lossy() == name)
                    .then(|| percent_decode_str(value).decode_utf8_lossy().into_owned())
            })
            .collect();
        return (!values.is_empty()).then(|| values.join(","));
    }
    if let Some(name) = source.strip_prefix("method.request.header.") {
        return req
            .headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
    }
    if let Some(name) = source.strip_prefix("method.request.multivalueheader.") {
        let values: Vec<&str> = req
            .headers
            .get_all(name)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .collect();
        return (!values.is_empty()).then(|| values.join(","));
    }
    if let Some(name) = source.strip_prefix("stageVariables.") {
        return stage_vars.get(name).cloned();
    }
    if let Some(path) = source.strip_prefix("context.") {
        let mut node = context;
        for part in path.split('.') {
            node = node.get(part)?;
        }
        return match node {
            Value::Null => None,
            Value::String(s) => Some(s.clone()),
            other => Some(other.to_string()),
        };
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn integration(parameters: &[(&str, &str)]) -> Integration {
        Integration {
            rest_api_id: "api".to_string(),
            resource_id: "res".to_string(),
            http_method: "ANY".to_string(),
            integration_type: "HTTP_PROXY".to_string(),
            integration_http_method: Some("ANY".to_string()),
            uri: Some("http://backend.internal/{proxy}".to_string()),
            credentials: None,
            request_parameters: parameters
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            request_templates: BTreeMap::new(),
            passthrough_behavior: "WHEN_NO_MATCH".to_string(),
            timeout_in_millis: None,
            cache_namespace: None,
            cache_key_parameters: vec![],
            content_handling: None,
            connection_type: None,
            connection_id: None,
            tls_config: None,
        }
    }

    fn request(headers: &[(&str, &str)], query: &str) -> AwsRequest {
        let mut map = http::HeaderMap::new();
        for (k, v) in headers {
            map.append(
                http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                http::HeaderValue::from_str(v).unwrap(),
            );
        }
        AwsRequest {
            service: "apigateway".to_string(),
            action: String::new(),
            region: "us-east-1".to_string(),
            account_id: "123456789012".to_string(),
            request_id: "req-1".to_string(),
            headers: map,
            query_params: query
                .split('&')
                .filter_map(|p| p.split_once('='))
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            body: bytes::Bytes::new(),
            body_stream: parking_lot::Mutex::new(None),
            path_segments: vec![],
            raw_path: "/x".to_string(),
            raw_query: query.to_string(),
            method: Method::GET,
            is_query_protocol: false,
            access_key_id: None,
            principal: None,
        }
    }

    fn build(parameters: &[(&str, &str)], req: &AwsRequest) -> MappedRequest {
        let path_params: BTreeMap<String, String> =
            [("proxy".to_string(), "a b/c".to_string())].into();
        let stage_vars: BTreeMap<String, String> = [("env".to_string(), "dev".to_string())].into();
        let context =
            json!({"identity": {"sourceIp": "10.0.0.1", "user": null}, "requestId": "r1"});
        MappedRequest::build(
            &integration(parameters),
            req,
            &path_params,
            &stage_vars,
            &context,
        )
    }

    #[test]
    fn a_greedy_path_parameter_fills_the_uri_and_keeps_its_slashes() {
        let mapped = build(
            &[(
                "integration.request.path.proxy",
                "method.request.path.proxy",
            )],
            &request(&[], ""),
        );
        assert_eq!(
            mapped.url_for("http://backend.internal/{proxy}", None),
            "http://backend.internal/a%20b/c"
        );
        assert_eq!(
            mapped.url_for("http://backend.internal/{proxy+}", None),
            "http://backend.internal/a%20b/c"
        );
    }

    #[test]
    fn headers_come_from_every_source_kind() {
        let mapped = build(
            &[
                (
                    "integration.request.header.x-ip",
                    "context.identity.sourceIp",
                ),
                ("integration.request.header.x-env", "stageVariables.env"),
                ("integration.request.header.x-static", "'fixed value'"),
                (
                    "integration.request.header.x-in",
                    "method.request.header.x-client",
                ),
                (
                    "integration.request.header.x-multi",
                    "method.request.multivalueheader.x-m",
                ),
                (
                    "integration.request.header.x-q",
                    "method.request.querystring.q",
                ),
            ],
            &request(&[("x-client", "c1"), ("x-m", "one"), ("x-m", "two")], "q=7"),
        );
        let headers: BTreeMap<_, _> = mapped.headers().iter().cloned().collect();
        assert_eq!(headers["x-ip"], "10.0.0.1");
        assert_eq!(headers["x-env"], "dev");
        assert_eq!(headers["x-static"], "fixed value");
        assert_eq!(headers["x-in"], "c1");
        assert_eq!(headers["x-multi"], "one,two");
        assert_eq!(headers["x-q"], "7");
    }

    #[test]
    fn a_source_that_resolves_to_nothing_is_left_out() {
        // `context.identity.user` is null and the other two name things the request lacks.
        let mapped = build(
            &[
                ("integration.request.header.x-user", "context.identity.user"),
                (
                    "integration.request.header.x-absent",
                    "method.request.header.nope",
                ),
                ("integration.request.header.x-stage", "stageVariables.nope"),
            ],
            &request(&[], ""),
        );
        assert!(mapped.headers().is_empty());
    }

    #[test]
    fn a_proxy_forwards_the_client_query_and_a_mapped_parameter_replaces_its_namesake() {
        let mapped = build(
            &[("integration.request.querystring.page", "'2'")],
            &request(&[], "page=1&sort=asc"),
        );
        assert_eq!(
            mapped.url_for("http://b/x", Some("page=1&sort=asc")),
            "http://b/x?sort=asc&page=2"
        );
        // Without forwarding (a non-proxy integration) only the mapped parameter goes.
        assert_eq!(mapped.url_for("http://b/x", None), "http://b/x?page=2");
        // A URI that already has a query string is extended, not restarted.
        assert_eq!(
            mapped.url_for("http://b/x?a=1", None),
            "http://b/x?a=1&page=2"
        );
    }

    #[test]
    fn with_nothing_mapped_the_uri_is_unchanged_apart_from_the_forwarded_query() {
        let mapped = build(&[], &request(&[], ""));
        assert_eq!(mapped.url_for("http://b/x", None), "http://b/x");
        assert_eq!(mapped.url_for("http://b/x", Some("a=1")), "http://b/x?a=1");
    }
}
