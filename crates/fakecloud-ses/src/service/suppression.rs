use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use http::StatusCode;
use serde_json::{json, Value};

use fakecloud_core::service::{AwsRequest, AwsResponse, AwsServiceError};

use crate::state::SesState;
use crate::state::SuppressedDestination;

use super::SesV2Service;

/// Which suppression list an operation targets: the account-level list, or
/// the list of the tenant named by the optional `TenantName` member.
enum ListScope {
    Account,
    Tenant(String),
}

impl ListScope {
    /// Resolve the optional `TenantName` member. An empty name violates the
    /// model's `@length(min: 1)` and is rejected with `BadRequestException`.
    fn parse(tenant_name: Option<&str>) -> Result<Self, AwsServiceError> {
        match tenant_name {
            None => Ok(Self::Account),
            Some("") => Err(AwsServiceError::aws_error(
                StatusCode::BAD_REQUEST,
                "BadRequestException",
                "TenantName must be at least 1 character long",
            )),
            Some(name) => Ok(Self::Tenant(name.to_string())),
        }
    }

    /// The scoped list for reads. `Err` (NotFoundException) when the tenant
    /// does not exist.
    fn list<'a>(
        &self,
        state: &'a SesState,
    ) -> Result<Option<&'a BTreeMap<String, SuppressedDestination>>, AwsServiceError> {
        match self {
            Self::Account => Ok(Some(&state.suppressed_destinations)),
            Self::Tenant(name) => {
                if !state.tenants.contains_key(name) {
                    return Err(tenant_not_found(name));
                }
                Ok(state.tenant_suppressed_destinations.get(name))
            }
        }
    }

    /// The scoped list for writes, created on demand for an existing tenant.
    fn list_mut<'a>(
        &self,
        state: &'a mut SesState,
    ) -> Result<&'a mut BTreeMap<String, SuppressedDestination>, AwsServiceError> {
        match self {
            Self::Account => Ok(&mut state.suppressed_destinations),
            Self::Tenant(name) => {
                if !state.tenants.contains_key(name) {
                    return Err(tenant_not_found(name));
                }
                Ok(state
                    .tenant_suppressed_destinations
                    .entry(name.clone())
                    .or_default())
            }
        }
    }
}

fn tenant_not_found(name: &str) -> AwsServiceError {
    AwsServiceError::aws_error(
        StatusCode::NOT_FOUND,
        "NotFoundException",
        format!("Tenant {name} does not exist"),
    )
}

fn not_suppressed(email: &str) -> AwsResponse {
    SesV2Service::json_error(
        StatusCode::NOT_FOUND,
        "NotFoundException",
        &format!("{} is not on the suppression list", email),
    )
}

/// Parse a `StartDate` / `EndDate` query timestamp (ISO 8601 date-time, the
/// restJson1 query default, or epoch seconds).
fn parse_query_date(field: &str, raw: &str) -> Result<DateTime<Utc>, AwsServiceError> {
    if let Ok(t) = DateTime::parse_from_rfc3339(raw) {
        return Ok(t.with_timezone(&Utc));
    }
    raw.parse::<f64>()
        .ok()
        .filter(|secs| secs.is_finite())
        .and_then(|secs| DateTime::from_timestamp_millis((secs * 1000.0) as i64))
        .ok_or_else(|| {
            AwsServiceError::aws_error(
                StatusCode::BAD_REQUEST,
                "BadRequestException",
                format!("{field} is not a valid timestamp"),
            )
        })
}

impl SesV2Service {
    pub(super) fn put_suppressed_destination(
        &self,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        let body: Value = Self::parse_body(req)?;
        let email = match body["EmailAddress"].as_str() {
            Some(e) => e.to_string(),
            None => {
                return Ok(Self::json_error(
                    StatusCode::BAD_REQUEST,
                    "BadRequestException",
                    "EmailAddress is required",
                ));
            }
        };
        let reason = match body["Reason"].as_str() {
            Some(r) if r == "BOUNCE" || r == "COMPLAINT" => r.to_string(),
            Some(_) => {
                return Ok(Self::json_error(
                    StatusCode::BAD_REQUEST,
                    "BadRequestException",
                    "Reason must be BOUNCE or COMPLAINT",
                ));
            }
            None => {
                return Ok(Self::json_error(
                    StatusCode::BAD_REQUEST,
                    "BadRequestException",
                    "Reason is required",
                ));
            }
        };
        let scope = ListScope::parse(body["TenantName"].as_str())?;

        let mut accounts = self.state.write();
        let state = accounts.get_or_create(&req.account_id);
        let list = scope.list_mut(state)?;
        list.insert(
            email.clone(),
            SuppressedDestination {
                email_address: email,
                reason,
                last_update_time: Utc::now(),
            },
        );

        Ok(AwsResponse::json(StatusCode::OK, "{}"))
    }

    pub(super) fn get_suppressed_destination(
        &self,
        email: &str,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        let scope = ListScope::parse(req.query_params.get("TenantName").map(String::as_str))?;
        let accounts = self.state.read();
        let empty = SesState::new(&req.account_id, &req.region);
        let state = accounts.get(&req.account_id).unwrap_or(&empty);
        let dest = match scope.list(state)?.and_then(|l| l.get(email)) {
            Some(d) => d,
            None => return Ok(not_suppressed(email)),
        };

        let response = json!({
            "SuppressedDestination": {
                "EmailAddress": dest.email_address,
                "Reason": dest.reason,
                "LastUpdateTime": dest.last_update_time.timestamp() as f64,
            }
        });

        Ok(AwsResponse::json(StatusCode::OK, response.to_string()))
    }

    pub(super) fn delete_suppressed_destination(
        &self,
        email: &str,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        let scope = ListScope::parse(req.query_params.get("TenantName").map(String::as_str))?;
        let mut accounts = self.state.write();
        let state = accounts.get_or_create(&req.account_id);
        // Lookup only: a miss must not materialize an empty tenant list.
        let list = match &scope {
            ListScope::Account => Some(&mut state.suppressed_destinations),
            ListScope::Tenant(name) => {
                if !state.tenants.contains_key(name) {
                    return Err(tenant_not_found(name));
                }
                state.tenant_suppressed_destinations.get_mut(name)
            }
        };
        if list.and_then(|l| l.remove(email)).is_none() {
            return Ok(not_suppressed(email));
        }
        Ok(AwsResponse::json(StatusCode::OK, "{}"))
    }

    pub(super) fn list_suppressed_destinations(
        &self,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        let scope = ListScope::parse(req.query_params.get("TenantName").map(String::as_str))?;
        let params = Self::list_request_params(req, &[], None)?;
        // `Reason` is a repeated query key; accept a comma-joined value too.
        let reasons: Vec<&str> = req
            .query_params
            .get("Reason")
            .map(|r| {
                r.split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        if let Some(bad) = reasons
            .iter()
            .find(|r| !matches!(**r, "BOUNCE" | "COMPLAINT"))
        {
            return Ok(Self::json_error(
                StatusCode::BAD_REQUEST,
                "BadRequestException",
                &format!("Invalid Reason {bad}: must be BOUNCE or COMPLAINT"),
            ));
        }
        let mut bounds = [None, None];
        for (slot, field) in bounds.iter_mut().zip(["StartDate", "EndDate"]) {
            if let Some(raw) = req.query_params.get(field) {
                *slot = Some(parse_query_date(field, raw)?);
            }
        }
        let [start, end] = bounds;

        let accounts = self.state.read();
        let empty = SesState::new(&req.account_id, &req.region);
        let state = accounts.get(&req.account_id).unwrap_or(&empty);
        let list = scope.list(state)?;
        let items: Vec<(String, Value)> = list
            .into_iter()
            .flatten()
            .filter(|(_, d)| reasons.is_empty() || reasons.contains(&d.reason.as_str()))
            .filter(|(_, d)| start.is_none_or(|s| d.last_update_time >= s))
            .filter(|(_, d)| end.is_none_or(|e| d.last_update_time <= e))
            .map(|(email, d)| {
                (
                    email.clone(),
                    json!({
                        "EmailAddress": d.email_address,
                        "Reason": d.reason,
                        "LastUpdateTime": d.last_update_time.timestamp() as f64,
                    }),
                )
            })
            .collect();
        let (page, next) = params.page(items);

        let mut response = json!({
            "SuppressedDestinationSummaries": page,
        });
        if let Some(token) = next {
            response["NextToken"] = json!(token);
        }

        Ok(AwsResponse::json(StatusCode::OK, response.to_string()))
    }
}
