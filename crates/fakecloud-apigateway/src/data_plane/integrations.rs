//! apigateway data_plane `integrations` concerns (audit-2026-05-19).

use super::*;

/// Resolve the outgoing backend method for a **non-proxy** `HTTP`
/// integration: AWS calls the backend with the configured
/// `integrationHttpMethod`, not the client's front-facing method (a
/// `GET` resource can be wired to `POST` the backend). Returns `None`
/// when unset so callers keep the client method as the fallback.
pub(super) fn integration_method_override(integration: &Integration) -> Option<Method> {
    integration
        .integration_http_method
        .as_deref()
        .and_then(|m| Method::from_bytes(m.to_ascii_uppercase().as_bytes()).ok())
}

/// Call an `HTTP` / `HTTP_PROXY` integration's backend, with its `requestParameters` applied: `{name}` in the URI filled
/// from the mapped path parameters, the mapped query string and headers added, and, for a proxy
/// integration, the client's own query string passed through.
pub(super) async fn http_proxy_mapped(
    req: &AwsRequest,
    integration: &Integration,
    mapped: &MappedRequest,
    body_override: Option<bytes::Bytes>,
    method_override: Option<Method>,
) -> Result<AwsResponse, AwsServiceError> {
    let uri = integration
        .uri
        .as_ref()
        .ok_or_else(|| bad_gateway("HTTP integration missing uri"))?;
    let forward_query = (integration.integration_type == "HTTP_PROXY" && !req.raw_query.is_empty())
        .then_some(req.raw_query.as_str());
    let url = mapped.url_for(uri, forward_query);
    let effective_method = method_override.as_ref().unwrap_or(&req.method);
    let method = match *effective_method {
        Method::GET => reqwest::Method::GET,
        Method::POST => reqwest::Method::POST,
        Method::PUT => reqwest::Method::PUT,
        Method::DELETE => reqwest::Method::DELETE,
        Method::PATCH => reqwest::Method::PATCH,
        Method::HEAD => reqwest::Method::HEAD,
        Method::OPTIONS => reqwest::Method::OPTIONS,
        _ => reqwest::Method::GET,
    };
    // API Gateway enforces a per-integration timeout (default 29s for REST,
    // configurable via timeoutInMillis up to that ceiling). Without it a hung
    // backend would hold the execute-api request open forever.
    let timeout_ms = integration
        .timeout_in_millis
        .filter(|v| *v > 0)
        .unwrap_or(29_000) as u64;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(timeout_ms))
        .build()
        .map_err(|e| bad_gateway(format!("failed to build HTTP client: {e}")))?;
    let mut builder = client.request(method, url);
    // The mapped headers replace a client header of the same name rather than joining it.
    let mapped_names: Vec<String> = mapped
        .headers()
        .iter()
        .map(|(name, _)| name.to_ascii_lowercase())
        .collect();
    for (k, v) in req.headers.iter() {
        if mapped_names.iter().any(|n| n == k.as_str()) {
            continue;
        }
        if let Ok(s) = v.to_str() {
            builder = builder.header(k.as_str(), s);
        }
    }
    for (name, value) in mapped.headers() {
        builder = builder.header(name.as_str(), value.as_str());
    }
    let body = body_override.as_ref().unwrap_or(&req.body);
    if !body.is_empty() {
        builder = builder.body(body.clone().to_vec());
    }
    let resp = builder.send().await.map_err(|e| {
        if e.is_timeout() {
            gateway_timeout(format!(
                "backend integration timed out after {timeout_ms} ms"
            ))
        } else {
            bad_gateway(format!("backend HTTP failure: {e}"))
        }
    })?;
    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut headers = http::HeaderMap::new();
    for (k, v) in resp.headers().iter() {
        if let (Ok(name), Ok(val)) = (
            http::HeaderName::from_bytes(k.as_str().as_bytes()),
            http::HeaderValue::from_bytes(v.as_bytes()),
        ) {
            // `append` preserves multi-value headers like multiple
            // `Set-Cookie` lines that the backend may emit.
            headers.append(name, val);
        }
    }
    let content_type = headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_string();
    let body = resp
        .bytes()
        .await
        .map_err(|e| bad_gateway(format!("backend body read failure: {e}")))?;
    Ok(AwsResponse {
        status,
        content_type,
        headers,
        body: bytes::Bytes::from(body.to_vec()).into(),
    })
}

/// A backend picked from a network load balancer's listener.
struct NlbTarget {
    target_group_arn: String,
    target_id: String,
    port: u16,
    load_balancer_security_groups: Vec<String>,
}

/// The first healthy registered target (else, failing open, the first registered one) behind the listener on `listener_port` of the first of
/// `target_arns` that is a load balancer with such a listener. `Ok(None)` when none has one, which
/// leaves the caller's own "no data plane" error to say so; `Err` when a listener exists and
/// nothing is registered behind it.
// ponytail: always the first usable target, not round-robin. A shared counter is the upgrade when
// a test needs to see traffic spread across several targets.
fn nlb_target(
    state: &fakecloud_elbv2::Elbv2State,
    target_arns: &[String],
    listener_port: u16,
) -> Result<Option<NlbTarget>, AwsServiceError> {
    for arn in target_arns {
        let Some(lb) = state.load_balancers.get(arn) else {
            continue;
        };
        let Some(listener) = state
            .listeners
            .values()
            .find(|l| l.load_balancer_arn == lb.arn && l.port == Some(i32::from(listener_port)))
        else {
            continue;
        };
        let mut group_arns: Vec<&str> = Vec::new();
        for action in &listener.default_actions {
            if let Some(forward) = &action.forward {
                group_arns.extend(
                    forward
                        .target_groups
                        .iter()
                        .map(|t| t.target_group_arn.as_str()),
                );
            }
            if let Some(group) = &action.target_group_arn {
                group_arns.push(group);
            }
        }
        for group_arn in group_arns {
            let Some(group) = state.target_groups.get(group_arn) else {
                continue;
            };
            // A healthy target if there is one. With none, the load balancer fails open and sends
            // to the registered targets anyway ("If all targets fail health checks at the same
            // time in all enabled Availability Zones, the load balancer fails open",
            // <https://docs.aws.amazon.com/elasticloadbalancing/latest/network/target-group-health-checks.md>,
            // fetched 2026-10-21), so a backend that is still starting answers with its own error
            // or a refused connection instead of this one.
            let usable = group
                .targets
                .iter()
                .find(|t| t.health.state != "unhealthy" && t.health.state != "unused")
                .or_else(|| group.targets.first());
            if let Some(target) = usable {
                let port = target
                    .port
                    .or(group.port)
                    .and_then(|p| u16::try_from(p).ok())
                    .ok_or_else(|| bad_gateway("VPC_LINK: target has no valid port"))?;
                return Ok(Some(NlbTarget {
                    target_group_arn: group.arn.clone(),
                    target_id: target.id.clone(),
                    port,
                    load_balancer_security_groups: lb.security_groups.clone(),
                }));
            }
        }
        return Err(bad_gateway(format!(
            "VPC_LINK: nothing is registered behind port {listener_port} of {}",
            lb.name
        )));
    }
    Ok(None)
}

/// Resolve a VPC_LINK integration by looking up the VpcLink's `targetArns` and forwarding the
/// request to what they front:
///
/// 1. a load balancer the ELBv2 data plane has bound a port for (an ALB), or else
/// 2. a network load balancer, which has no bound port: the listener on the integration URI's port
///    (default 80), its forward action's target group, and the first registered target that is
///    not `unhealthy` or `unused`, as the ALB data plane does.
///
/// The integration's `requestParameters` are applied as for [`http_proxy_mapped`].
pub(super) async fn vpc_link_proxy_mapped(
    req: &AwsRequest,
    integration: &Integration,
    mapped: &MappedRequest,
    method_override: Option<Method>,
    service: &ApiGatewayService,
) -> Result<AwsResponse, AwsServiceError> {
    let connection_id = integration
        .connection_id
        .as_deref()
        .ok_or_else(|| bad_gateway("VPC_LINK integration missing connectionId"))?;

    let target_arns: Vec<String> = {
        let accounts = service.state_handle().read();
        let state = accounts
            .get(&req.account_id)
            .ok_or_else(|| bad_gateway("VPC_LINK: account not found in API Gateway state"))?;
        let vpc_link = state
            .vpc_links
            .get(connection_id)
            .ok_or_else(|| bad_gateway(format!("VPC_LINK not found: {connection_id}")))?;
        let target_arns = vpc_link
            .get("targetArns")
            .and_then(|v| v.as_array())
            .ok_or_else(|| bad_gateway("VPC_LINK missing targetArns"))?;
        if target_arns.is_empty() {
            return Err(bad_gateway("VPC_LINK targetArns is empty"));
        }
        target_arns
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect()
    };

    let original_url = integration.uri.as_deref().unwrap_or("http://localhost/");
    let listener_port = http::Uri::try_from(original_url)
        .ok()
        .and_then(|u| u.port_u16())
        .unwrap_or(80);

    let elbv2 = service
        .elbv2_state
        .as_ref()
        .ok_or_else(|| bad_gateway("VPC_LINK not available: ELBv2 state not wired"))?;
    let (bound_port, nlb) = {
        let elbv2_read = elbv2.read();
        let elbv2_account = elbv2_read
            .get(&req.account_id)
            .ok_or_else(|| bad_gateway("VPC_LINK: account not found in ELBv2 state"))?;
        let bound_port = target_arns
            .iter()
            .filter_map(|arn| elbv2_account.load_balancers.get(arn))
            .find_map(|lb| lb.bound_port);
        let nlb = if bound_port.is_none() {
            nlb_target(elbv2_account, &target_arns, listener_port)?
        } else {
            None
        };
        (bound_port, nlb)
    };
    let backend = match (bound_port, nlb) {
        (Some(port), _) => format!("http://127.0.0.1:{port}"),
        (None, Some(target)) => {
            let account =
                fakecloud_core::dataplane::account_of_arn(&target.target_group_arn).unwrap_or_default();
            let endpoint = fakecloud_core::dataplane::resolve_target(
                account,
                &target.target_id,
                target.port,
                &sibling_host(),
                &target.load_balancer_security_groups,
            )
            .await;
            format!("http://{}:{}", endpoint.host, endpoint.port)
        }
        (None, None) => {
            return Err(bad_gateway(
                "VPC_LINK: none of the target NLBs/ALBs have an active dataplane port or a listener on the integration's port",
            ))
        }
    };

    // The integration URI's path and query, on the backend's address, with the mapped path
    // parameters filled in.
    let expanded = mapped.url_for(original_url, None);
    let path_and_query = http::Uri::try_from(expanded.as_str())
        .ok()
        .and_then(|u| u.path_and_query().map(|p| p.as_str().to_string()))
        .unwrap_or_else(|| req.raw_path.clone());
    let mut proxy_integration = integration.clone();
    proxy_integration.uri = Some(format!("{backend}{path_and_query}"));
    // The path is already filled in, so only the query and headers are left to map.
    let remaining = MappedRequest::without_path(mapped);
    http_proxy_mapped(req, &proxy_integration, &remaining, None, method_override).await
}

/// How fakecloud reaches a sibling it just started: the shared container networking's answer,
/// worked out once.
fn sibling_host() -> String {
    static HOST: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    HOST.get_or_init(|| {
        fakecloud_core::container_net::detect_container_cli()
            .map(|cli| fakecloud_core::container_net::HostNetworking::detect(&cli).sibling_host)
            .unwrap_or_else(|| "127.0.0.1".to_string())
    })
    .clone()
}

/// Apply the integration's `requestTemplates` to the request body.
/// Returns `Some(transformed_body)` when a template matched the request
/// content type, or `None` to leave the body unchanged.
pub(super) fn apply_request_template(
    req: &AwsRequest,
    integration: &Integration,
    vtl_ctx: &mut crate::vtl::Context,
) -> Option<bytes::Bytes> {
    let content_type = req
        .headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json");
    let normalized = content_type
        .split(';')
        .next()
        .unwrap_or(content_type)
        .trim();
    let template = integration.request_templates.get(normalized)?;
    let rendered = crate::vtl::render(template, vtl_ctx);
    Some(bytes::Bytes::from(rendered))
}

/// Apply the integration response's `responseTemplates` to the backend
/// response body. Returns a new `AwsResponse` with the rendered template.
#[allow(clippy::too_many_arguments)]
pub(super) async fn apply_response_template(
    backend_resp: AwsResponse,
    integration: &Integration,
    req: &AwsRequest,
    vtl_ctx: &mut crate::vtl::Context,
    api_id: &str,
    resource_path: &str,
    _stage_name: &str,
    _path_params: &BTreeMap<String, String>,
    _stage_vars: &BTreeMap<String, String>,
    service: &ApiGatewayService,
) -> Result<AwsResponse, AwsServiceError> {
    let status_code = backend_resp.status.as_u16().to_string();
    let key = response_key(
        api_id,
        resource_path,
        &integration.http_method,
        &status_code,
    );
    // PutIntegrationResponse (and an OpenAPI import) key the response by the
    // resource id; the resource path form is accepted too.
    let id_key = response_key(
        api_id,
        &integration.resource_id,
        &integration.http_method,
        &status_code,
    );
    let accounts = service.state_handle().read();
    let state = accounts.get(&req.account_id);
    let resp_record = state.and_then(|st| {
        st.integration_responses
            .get(&id_key)
            .or_else(|| st.integration_responses.get(&key))
    });
    let resp_template = resp_record
        .and_then(|v| v.get("responseTemplates"))
        .and_then(|t| t.as_object());
    let content_type = backend_resp
        .content_type
        .as_str()
        .split(';')
        .next()
        .unwrap_or(&backend_resp.content_type)
        .trim();
    let template = resp_template
        .and_then(|t| t.get(content_type))
        .and_then(|v| v.as_str());
    let body = if let Some(template) = template {
        // Inject $input with the backend response body so the template
        // can reference it.
        let body_str = String::from_utf8_lossy(backend_resp.body.expect_bytes()).to_string();
        let body_json: Value = serde_json::from_str(&body_str).unwrap_or(Value::Null);
        vtl_ctx.set("input", json!({"body": body_str, "json": body_json}));
        crate::vtl::render(template, vtl_ctx).into_bytes()
    } else {
        backend_resp.body.expect_bytes().to_vec()
    };
    // Apply the integration response's responseParameters (mapped/static
    // headers) onto the client response. Source expressions read the
    // original backend headers, so clone before mutating.
    let mut headers = backend_resp.headers.clone();
    apply_response_parameters(&mut headers, resp_record, Some(&backend_resp));
    Ok(AwsResponse {
        status: backend_resp.status,
        content_type: backend_resp.content_type,
        headers,
        body: bytes::Bytes::from(body).into(),
    })
}

/// Apply an integration response's `responseParameters` map to the
/// outgoing response headers. Keys are `method.response.header.<Name>`;
/// values are either a `'literal'` (single-quoted static value) or a
/// mapping expression `integration.response.header.<X>` sourced from the
/// backend response. This is what makes a MOCK-based CORS preflight
/// return its `Access-Control-Allow-*` headers, and lets non-proxy HTTP
/// integrations project backend headers onto the client response.
fn apply_response_parameters(
    headers: &mut http::HeaderMap,
    resp_record: Option<&Value>,
    backend: Option<&AwsResponse>,
) {
    let Some(params) = resp_record
        .and_then(|r| r.get("responseParameters"))
        .and_then(|v| v.as_object())
    else {
        return;
    };
    for (key, value) in params {
        let Some(header_name) = key.strip_prefix("method.response.header.") else {
            continue;
        };
        let raw = value.as_str().unwrap_or_default();
        let resolved = if raw.len() >= 2 && raw.starts_with('\'') && raw.ends_with('\'') {
            Some(raw[1..raw.len() - 1].to_string())
        } else if let Some(src) = raw.strip_prefix("integration.response.header.") {
            backend
                .and_then(|b| b.headers.get(src))
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string())
        } else if !raw.is_empty() {
            Some(raw.to_string())
        } else {
            None
        };
        if let (Ok(name), Some(v)) = (
            http::HeaderName::from_bytes(header_name.as_bytes()),
            resolved
                .as_deref()
                .and_then(|v| http::HeaderValue::from_str(v).ok()),
        ) {
            headers.insert(name, v);
        }
    }
}

/// Build a MOCK integration response by looking up the integration
/// response for the method and rendering its `responseTemplates`.
#[allow(clippy::too_many_arguments)]
pub(super) async fn mock_response(
    req: &AwsRequest,
    integration: &Integration,
    vtl_ctx: &mut crate::vtl::Context,
    api_id: &str,
    resource_path: &str,
    _stage_name: &str,
    service: &ApiGatewayService,
) -> Result<AwsResponse, AwsServiceError> {
    let method = integration.http_method.as_str();
    // MOCK integrations select the integration response via the request
    // template, which conventionally emits `{"statusCode": NNN}`. Render
    // the application/json request template and read that selector; fall
    // back to 200 when there is no template or no statusCode field.
    let default_status = "200";
    let selected_status = integration
        .request_templates
        .get("application/json")
        .map(|tpl| crate::vtl::render(tpl, vtl_ctx))
        .and_then(|rendered| serde_json::from_str::<Value>(&rendered).ok())
        .and_then(|v| v.get("statusCode").and_then(|s| s.as_i64()))
        .map(|n| n.to_string());
    let lookup_status = selected_status.as_deref().unwrap_or(default_status);
    let key = response_key(api_id, resource_path, method, lookup_status);
    let accounts = service.state_handle().read();
    let state = accounts.get(&req.account_id);
    // Try the selected-status response first; if absent scan for any
    // integration response registered for this method and use its status.
    // PutIntegrationResponse (and an OpenAPI import) key the response by the
    // resource id; the resource path form is accepted too.
    let id_key = response_key(api_id, &integration.resource_id, method, lookup_status);
    let resp_record = state.and_then(|st| {
        st.integration_responses
            .get(&id_key)
            .or_else(|| st.integration_responses.get(&key))
            .or_else(|| {
                [&integration.resource_id, &resource_path.to_string()]
                    .into_iter()
                    .find_map(|res| {
                        let prefix = format!("{api_id}/{res}/{method}/");
                        st.integration_responses
                            .iter()
                            .find(|(k, _)| k.starts_with(&prefix))
                            .map(|(_, v)| v)
                    })
            })
    });
    let (status, resp_templates) = if let Some(record) = resp_record {
        let status = record
            .get("statusCode")
            .and_then(|v| v.as_str())
            .unwrap_or(lookup_status);
        let templates = record.get("responseTemplates").and_then(|v| v.as_object());
        (status, templates)
    } else {
        (lookup_status, None)
    };
    let status = status
        .parse::<u16>()
        .ok()
        .and_then(|n| StatusCode::from_u16(n).ok())
        .unwrap_or(StatusCode::OK);
    let content_type = "application/json";
    let body = if let Some(templates) = resp_templates {
        templates
            .get(content_type)
            .and_then(|v| v.as_str())
            .map(|t| crate::vtl::render(t, vtl_ctx))
            .unwrap_or_default()
    } else {
        String::new()
    };
    // Apply responseParameters (e.g. static CORS headers) to the response.
    let mut headers = http::HeaderMap::new();
    apply_response_parameters(&mut headers, resp_record, None);
    Ok(AwsResponse {
        status,
        content_type: content_type.to_string(),
        headers,
        body: bytes::Bytes::from(body).into(),
    })
}

/// Dispatch an `AWS` direct service integration to the corresponding
/// fakecloud service handler. The integration URI follows the API Gateway
/// format: `arn:aws:apigateway:{region}:{service}:action/{Action}` or
/// `arn:aws:apigateway:{region}:{service}:path/{path}`.
pub(super) async fn aws_direct_integration(
    req: &AwsRequest,
    uri: &str,
    integration: &Integration,
    service: &ApiGatewayService,
) -> Result<AwsResponse, AwsServiceError> {
    let registry = service.registry().ok_or_else(|| {
        bad_gateway("AWS direct integration not available: service registry not wired")
    })?;
    let registry = registry.get().ok_or_else(|| {
        bad_gateway("AWS direct integration not available: service registry not yet populated")
    })?;

    // Split only the first 6 segments — the trailing one is `action/...`
    // or `path/...` and can carry embedded `:` (e.g. Lambda ARNs). Naive
    // `split(':').collect()` truncated those at the first colon and
    // mis-routed the integration.
    let parts: Vec<&str> = fakecloud_aws::arn::arn_resource(uri, "apigateway")
        .map(|rest| rest.splitn(3, ':').collect())
        .unwrap_or_default();
    if parts.len() < 3 {
        return Err(bad_gateway(format!(
            "AWS integration uri not in expected ARN format: {uri}"
        )));
    }
    // The URI's region is the region the target service is called in
    // (normally the API's own); an empty one means the API's region.
    let target_region = if parts[0].is_empty() {
        req.region.clone()
    } else {
        parts[0].to_string()
    };
    let target_service = parts[1];
    let action_or_path = parts[2];

    let target = registry.get(target_service).ok_or_else(|| {
        bad_gateway(format!(
            "AWS integration target service '{target_service}' not registered"
        ))
    })?;

    // For a non-proxy `AWS` integration the backend method is the one
    // configured on the integration (`integrationHttpMethod`), NOT the
    // client's front-facing method. On real AWS this is always `POST` for
    // Lambda invoke integrations, so a `GET /users` resource still reaches
    // Lambda over `POST`. Falling back to the client's method only when no
    // integration method is set preserves prior behavior for any caller
    // that omitted it.
    let dispatch_method = integration
        .integration_http_method
        .as_deref()
        .and_then(|m| Method::from_bytes(m.to_ascii_uppercase().as_bytes()).ok())
        .unwrap_or_else(|| req.method.clone());

    let mut dispatch_req = AwsRequest {
        service: target_service.to_string(),
        action: req.action.clone(),
        region: target_region,
        account_id: req.account_id.clone(),
        request_id: uuid::Uuid::new_v4().to_string(),
        headers: req.headers.clone(),
        query_params: req.query_params.clone(),
        body: req.body.clone(),
        body_stream: parking_lot::Mutex::new(None),
        // The target service expects dispatch-shaped (decoded) segments.
        path_segments: fakecloud_core::path::split_path_segments(&req.raw_path),
        raw_path: req.raw_path.clone(),
        raw_query: req.raw_query.clone(),
        method: dispatch_method,
        is_query_protocol: false,
        access_key_id: req.access_key_id.clone(),
        principal: req.principal.clone(),
    };

    if let Some(action) = action_or_path.strip_prefix("action/") {
        dispatch_req.action = action.to_string();
    } else if let Some(path) = action_or_path.strip_prefix("path/") {
        dispatch_req.raw_path = format!("/{path}");
        dispatch_req.path_segments = fakecloud_core::path::split_path_segments(path);
    } else {
        return Err(bad_gateway(format!(
            "AWS integration uri must contain action/ or path/ segment: {uri}"
        )));
    }

    target.handle(dispatch_req).await
}
