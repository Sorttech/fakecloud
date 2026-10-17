//! `LambdaService` `account` family — extracted from service.rs by audit-2026-05-19.

use super::*;

/// Operations addressed at one function's sub-resources. Each declares
/// `ResourceNotFoundException` in the Smithy model, and each 404s on AWS when
/// the function itself is absent.
const FUNCTION_SCOPED_ACTIONS: &[&str] = &[
    "CreateAlias",
    "CreateFunctionUrlConfig",
    "DeleteAlias",
    "DeleteFunctionCodeSigningConfig",
    "DeleteFunctionConcurrency",
    "DeleteFunctionEventInvokeConfig",
    "DeleteFunctionUrlConfig",
    "DeleteProvisionedConcurrencyConfig",
    "GetAlias",
    "GetFunctionCodeSigningConfig",
    "GetFunctionConcurrency",
    "GetFunctionEventInvokeConfig",
    "GetFunctionRecursionConfig",
    "GetFunctionScalingConfig",
    "GetFunctionUrlConfig",
    "GetProvisionedConcurrencyConfig",
    "GetRuntimeManagementConfig",
    "ListAliases",
    "ListFunctionEventInvokeConfigs",
    "ListFunctionUrlConfigs",
    "ListProvisionedConcurrencyConfigs",
    "ListVersionsByFunction",
    "PutFunctionCodeSigningConfig",
    "PutFunctionConcurrency",
    "PutFunctionEventInvokeConfig",
    "PutFunctionRecursionConfig",
    "PutFunctionScalingConfig",
    "PutProvisionedConcurrencyConfig",
    "PutRuntimeManagementConfig",
    "UpdateAlias",
    "UpdateFunctionEventInvokeConfig",
    "UpdateFunctionUrlConfig",
];

impl LambdaService {
    pub(crate) async fn handle_extra(
        &self,
        action: &str,
        resource: Option<&str>,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        let aid = req.account_id.as_str();
        let rg = req.region.as_str();
        let res = resource.unwrap_or("");
        // Every operation scoped to a function declares
        // `ResourceNotFoundException`, and AWS raises it before touching the
        // sub-resource: reading or clearing an alias, a concurrency setting or
        // a URL config on a function that does not exist is a 404, not a
        // silent success. Guarding once here covers every operation in the
        // list, including the ones whose handlers never checked; the handful
        // that do check for themselves keep doing so, which costs a map lookup
        // and keeps them correct if they are ever called off this path.
        if FUNCTION_SCOPED_ACTIONS.contains(&action) {
            let exists = self
                .state
                .read()
                .regional(aid, rg)
                .is_some_and(|s| s.functions.contains_key(res));
            if !exists {
                return Err(not_found("Function", res));
            }
        }
        match action {
            // Function lifecycle extras
            "GetFunctionConfiguration" => self.get_function_configuration(res, aid, req),
            "UpdateFunctionConfiguration" => self.update_function_configuration(res, req),
            "UpdateFunctionCode" => self.update_function_code(res, req),
            "UpdateEventSourceMapping" => self.update_event_source_mapping_handler(res, req),
            "GetAccountSettings" => self.get_account_settings(aid, rg),
            "InvokeAsync" => Ok(AwsResponse::json(StatusCode::ACCEPTED, "{}".to_string())),
            "InvokeWithResponseStream" => {
                // The function ARN (or partial ARN) in the path names the
                // account invoked; a bare name is the caller's own function.
                let target = req
                    .path_segments
                    .get(2)
                    .and_then(|raw| crate::service::function_ref_scope(raw).0)
                    .unwrap_or(aid);
                self.invoke_with_response_stream(res, target, req).await
            }

            // Versions
            "ListVersionsByFunction" => self.list_versions_by_function(res, aid, req),

            // Aliases
            "CreateAlias" => self.create_alias(res, req),
            "GetAlias" => self.get_alias(res, req),
            "ListAliases" => self.list_aliases(res, aid, req),
            "UpdateAlias" => self.update_alias(res, req),
            "DeleteAlias" => self.delete_alias(res, req),

            // Layers
            "PublishLayerVersion" => self.publish_layer_version(req),
            "GetLayerVersion" => self.get_layer_version(req),
            "GetLayerVersionByArn" => self.get_layer_version_by_arn(req),
            "ListLayers" => {
                validate_layer_filters(req)?;
                self.list_layers(aid, req)
            }
            "ListLayerVersions" => {
                validate_layer_filters(req)?;
                self.list_layer_versions(aid, req)
            }
            "DeleteLayerVersion" => self.delete_layer_version(req),
            "GetLayerVersionPolicy" => self.get_layer_version_policy(req),
            "AddLayerVersionPermission" => self.add_layer_version_permission(req),
            "RemoveLayerVersionPermission" => self.remove_layer_version_permission(req),

            // Function URL
            "CreateFunctionUrlConfig" => self.create_function_url_config(res, req),
            "GetFunctionUrlConfig" => self.get_function_url_config(res, req),
            "UpdateFunctionUrlConfig" => self.update_function_url_config(res, req),
            "DeleteFunctionUrlConfig" => self.delete_function_url_config(res, req),
            "ListFunctionUrlConfigs" => self.list_function_url_configs(res, aid, rg),

            // Concurrency
            "PutFunctionConcurrency" => self.put_function_concurrency(res, req),
            "GetFunctionConcurrency" => self.get_function_concurrency(res, aid, rg),
            "DeleteFunctionConcurrency" => self.delete_function_concurrency(res, aid, rg),
            "PutProvisionedConcurrencyConfig" => self.put_provisioned_concurrency(res, req),
            "GetProvisionedConcurrencyConfig" => self.get_provisioned_concurrency(res, req),
            "DeleteProvisionedConcurrencyConfig" => self.delete_provisioned_concurrency(res, req),
            "ListProvisionedConcurrencyConfigs" => {
                self.list_provisioned_concurrency(res, aid, req.region.as_str())
            }

            // Code signing
            "CreateCodeSigningConfig" => self.create_code_signing_config(req),
            "GetCodeSigningConfig" => self.get_code_signing_config(res, aid, rg),
            "UpdateCodeSigningConfig" => self.update_code_signing_config(res, req),
            "DeleteCodeSigningConfig" => self.delete_code_signing_config(res, aid, rg),
            "ListCodeSigningConfigs" => self.list_code_signing_configs(aid, rg),
            "PutFunctionCodeSigningConfig" => self.put_function_code_signing(res, req),
            "GetFunctionCodeSigningConfig" => self.get_function_code_signing(res, aid, rg),
            "DeleteFunctionCodeSigningConfig" => self.delete_function_code_signing(res, aid, rg),
            "ListFunctionsByCodeSigningConfig" => self.list_functions_by_code_signing(res, aid, rg),

            // Event invoke
            "PutFunctionEventInvokeConfig" => self.put_function_event_invoke(res, req),
            "UpdateFunctionEventInvokeConfig" => self.update_function_event_invoke(res, req),
            "GetFunctionEventInvokeConfig" => self.get_function_event_invoke(res, req),
            "DeleteFunctionEventInvokeConfig" => self.delete_function_event_invoke(res, req),
            "ListFunctionEventInvokeConfigs" => self.list_function_event_invoke(res, aid, rg),

            // Runtime management
            "PutRuntimeManagementConfig" => self.put_runtime_management(res, req),
            "GetRuntimeManagementConfig" => self.get_runtime_management(res, req),

            // Scaling
            "PutFunctionScalingConfig" => self.put_scaling_config(res, req),
            "GetFunctionScalingConfig" => self.get_scaling_config(res, req),

            // Recursion
            "PutFunctionRecursionConfig" => self.put_recursion_config(res, req),
            "GetFunctionRecursionConfig" => self.get_recursion_config(res, aid, rg),

            // Tags
            "TagResource" => self.tag_resource(res, req),
            "UntagResource" => self.untag_resource(res, req),
            "ListTags" => self.list_tags(res, aid, rg),

            _ => Err(AwsServiceError::action_not_implemented("lambda", action)),
        }
    }

    pub(super) fn get_account_settings(
        &self,
        account_id: &str,
        region: &str,
    ) -> Result<AwsResponse, AwsServiceError> {
        // Account settings and usage are per region on AWS: the concurrency
        // limit, code storage and function count of one region never count
        // against another. With Service Quotas attached, the concurrency and
        // code storage limits are the account's applied quota values (read
        // before the Lambda lock: the lookup takes Service Quotas' locks).
        let concurrency_quota =
            self.applied_quota(account_id, region, crate::quota::CONCURRENT_EXECUTIONS);
        let storage_quota = self.applied_quota(account_id, region, crate::quota::CODE_STORAGE);
        let accounts = self.state.read();
        let empty = LambdaState::new(account_id, region);
        let state = accounts.regional(account_id, region).unwrap_or(&empty);
        // A stored setting of 0 is unset (a state migrated from an older
        // snapshot carries all-zero settings), not a limit of 0.
        let stored = state.account_settings.clone().unwrap_or_default();
        let settings = AccountSettings {
            concurrent_executions: crate::quota::concurrency_limit(state, concurrency_quota),
            code_size_zipped: crate::quota::setting_or(stored.code_size_zipped, 52_428_800),
            code_size_unzipped: crate::quota::setting_or(stored.code_size_unzipped, 262_144_000),
            total_code_size: crate::quota::code_storage_limit(state, storage_quota),
        };
        // Real AccountUsage so clients monitoring deployment quotas see
        // accurate numbers: AWS counts the code of every function, every
        // published version and every layer version.
        let function_count = state.functions.len() as i64;
        let total_code_size = crate::quota::code_storage_used(state);
        // UnreservedConcurrentExecutions = the account limit minus the
        // concurrency reserved by individual functions (AWS decrements it as
        // reserved concurrency is allocated).
        let reserved: i64 = state.function_concurrency.values().copied().sum();
        let unreserved = (settings.concurrent_executions - reserved).max(0);
        ok(json!({
            "AccountLimit": {
                "ConcurrentExecutions": settings.concurrent_executions,
                "CodeSizeZipped": settings.code_size_zipped,
                "CodeSizeUnzipped": settings.code_size_unzipped,
                "TotalCodeSize": settings.total_code_size,
                "UnreservedConcurrentExecutions": unreserved,
            },
            "AccountUsage": {
                "TotalCodeSize": total_code_size,
                "FunctionCount": function_count,
            },
        }))
    }
}
