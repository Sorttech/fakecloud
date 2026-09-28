// Auto-extracted from service.rs as part of carryover service.rs split.

#![allow(clippy::too_many_arguments)]

use serde_json::{json, Value};

use fakecloud_core::service::{AwsRequest, AwsResponse, AwsServiceError};

use super::*;
use crate::state::principal_settings_key;

/// Valid `SettingName` enum values from the ECS Smithy model. Used by all
/// {Put,PutDefault,Delete,List}AccountSettings* operations to validate the
/// `name` field — real ECS rejects anything outside this list.
const SETTING_NAME_VALUES: &[&str] = &[
    "serviceLongArnFormat",
    "taskLongArnFormat",
    "containerInstanceLongArnFormat",
    "awsvpcTrunking",
    "containerInsights",
    "fargateFIPSMode",
    "tagResourceAuthorization",
    "fargateTaskRetirementWaitPeriod",
    "guardDutyActivate",
    "defaultLogDriverMode",
    "fargateEventWindows",
];

/// Render a stored principal-settings key in `region`'s partition.
fn principal_in_region_partition(key: &str, region: &str) -> String {
    match key.parse::<Arn>() {
        Ok(arn) => arn
            .with_partition(fakecloud_aws::arn::partition_for(region))
            .to_string(),
        Err(_) => key.to_string(),
    }
}

impl EcsService {
    pub(super) fn put_account_setting(
        &self,
        request: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        let body = request.json_body();
        validate_enum_opt(&body, "name", SETTING_NAME_VALUES)?;
        let name = req_str(&body, "name")?.to_string();
        let value = req_str(&body, "value")?.to_string();
        let principal_arn = opt_str(&body, "principalArn")
            .map(String::from)
            .or_else(|| request.principal.as_ref().map(|p| p.arn.clone()))
            .unwrap_or_else(|| {
                Arn::global_in(&request.region, "iam", &request.account_id, "root").to_string()
            });
        let account = request.account_id.clone();
        let mut accounts = self.state.write();
        let state = accounts.get_or_create(&account);
        state
            .principal_account_settings
            .entry(principal_settings_key(&principal_arn))
            .or_default()
            .insert(name.clone(), value.clone());
        Ok(AwsResponse::ok_json(json!({
            "setting": {
                "name": name,
                "value": value,
                "principalArn": principal_arn,
            }
        })))
    }

    pub(super) fn put_account_setting_default(
        &self,
        request: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        let body = request.json_body();
        validate_enum_opt(&body, "name", SETTING_NAME_VALUES)?;
        let name = req_str(&body, "name")?.to_string();
        let value = req_str(&body, "value")?.to_string();
        let account = request.account_id.clone();
        let mut accounts = self.state.write();
        let state = accounts.get_or_create(&account);
        state
            .account_setting_defaults
            .insert(name.clone(), value.clone());
        Ok(AwsResponse::ok_json(json!({
            "setting": {
                "name": name,
                "value": value,
                "principalArn": Arn::global_in(&request.region, "iam", &state.account_id, "root")
                    .to_string(),
            }
        })))
    }

    pub(super) fn delete_account_setting(
        &self,
        request: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        let body = request.json_body();
        validate_enum_opt(&body, "name", SETTING_NAME_VALUES)?;
        let name = req_str(&body, "name")?.to_string();
        let principal_arn = opt_str(&body, "principalArn")
            .map(String::from)
            .or_else(|| request.principal.as_ref().map(|p| p.arn.clone()))
            .unwrap_or_else(|| {
                Arn::global_in(&request.region, "iam", &request.account_id, "root").to_string()
            });
        let account = request.account_id.clone();
        let mut accounts = self.state.write();
        let state = accounts.get_or_create(&account);
        let removed_value = state
            .principal_account_settings
            .get_mut(&principal_settings_key(&principal_arn))
            .and_then(|m| m.remove(&name));
        Ok(AwsResponse::ok_json(json!({
            "setting": {
                "name": name,
                "value": removed_value.unwrap_or_default(),
                "principalArn": principal_arn,
            }
        })))
    }

    pub(super) fn list_account_settings(
        &self,
        request: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        let body = request.json_body();
        validate_enum_opt(&body, "name", SETTING_NAME_VALUES)?;
        let name_filter = opt_str(&body, "name");
        let value_filter = opt_str(&body, "value");
        let principal_filter = opt_str(&body, "principalArn");
        let effective_only = body
            .get("effectiveSettings")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let account = request.account_id.clone();
        let accounts = self.state.read();
        let Some(state) = accounts.get(&account) else {
            return Ok(AwsResponse::ok_json(json!({"settings": []})));
        };
        let root_arn =
            Arn::global_in(&request.region, "iam", &state.account_id, "root").to_string();
        let principal_filter_key = principal_filter.map(principal_settings_key);
        let mut settings: Vec<Value> = Vec::new();

        if effective_only {
            // Merge principal overrides onto defaults, scoped to principal_filter
            // when supplied; otherwise use the caller's own principal.
            let principal = principal_filter
                .map(String::from)
                .or_else(|| request.principal.as_ref().map(|p| p.arn.clone()))
                .unwrap_or_else(|| root_arn.clone());
            let mut merged = state.account_setting_defaults.clone();
            if let Some(overrides) = state
                .principal_account_settings
                .get(&principal_settings_key(&principal))
            {
                for (k, v) in overrides {
                    merged.insert(k.clone(), v.clone());
                }
            }
            for (k, v) in merged {
                if matches_filter(name_filter, &k) && matches_filter(value_filter, &v) {
                    settings.push(json!({
                        "name": k,
                        "value": v,
                        "principalArn": principal,
                    }));
                }
            }
        } else {
            // Raw listing: include defaults (under the root ARN) plus any
            // principal-specific settings.
            for (k, v) in &state.account_setting_defaults {
                if matches_filter(name_filter, k)
                    && matches_filter(value_filter, v)
                    && principal_filter_key
                        .as_ref()
                        .is_none_or(|pf| *pf == principal_settings_key(&root_arn))
                {
                    settings.push(json!({
                        "name": k,
                        "value": v,
                        "principalArn": root_arn,
                    }));
                }
            }
            for (principal, entries) in &state.principal_account_settings {
                if principal_filter_key
                    .as_ref()
                    .is_some_and(|pf| pf != principal)
                {
                    continue;
                }
                let principal = principal_in_region_partition(principal, &request.region);
                for (k, v) in entries {
                    if matches_filter(name_filter, k) && matches_filter(value_filter, v) {
                        settings.push(json!({
                            "name": k,
                            "value": v,
                            "principalArn": principal,
                        }));
                    }
                }
            }
        }

        Ok(AwsResponse::ok_json(json!({"settings": settings})))
    }
}
