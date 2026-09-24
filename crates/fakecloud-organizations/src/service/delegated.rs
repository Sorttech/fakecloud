//! `OrganizationsService` `delegated` family — extracted from service.rs by audit-2026-05-19.

use super::*;

impl OrganizationsService {
    pub(super) fn register_delegated_administrator(
        &self,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        let body = req.json_body();
        let account_id = required_str(&body, "AccountId")?.to_string();
        let principal = required_str(&body, "ServicePrincipal")?.to_string();
        let mut guard = self.state.write();
        let org = self.management_org_mut(&mut guard, &req.account_id)?;
        org.register_delegated_administrator(&account_id, &principal)
            .map_err(org_error_to_aws)?;
        Ok(AwsResponse::ok_json(json!({})))
    }

    pub(super) fn deregister_delegated_administrator(
        &self,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        let body = req.json_body();
        let account_id = required_str(&body, "AccountId")?.to_string();
        let principal = required_str(&body, "ServicePrincipal")?.to_string();
        let mut guard = self.state.write();
        let org = self.management_org_mut(&mut guard, &req.account_id)?;
        org.deregister_delegated_administrator(&account_id, &principal)
            .map_err(org_error_to_aws)?;
        Ok(AwsResponse::ok_json(json!({})))
    }

    pub(super) fn list_delegated_administrators(
        &self,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        let body = req.json_body();
        let filter = body
            .get("ServicePrincipal")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let (max_results, next_token) = parse_list_pagination(&body)?;
        let guard = self.state.read();
        let org = self.management_or_delegated_org(&guard, &req.account_id)?;
        let entries: Vec<Value> = org
            .list_delegated_administrators(filter.as_deref())
            .into_iter()
            .filter_map(|admin| {
                let acct = org.accounts.get(&admin.account_id)?;
                Some(json!({
                    "Id": acct.id,
                    "Arn": acct.arn,
                    "Email": acct.email,
                    "Name": acct.name,
                    "Status": acct.status,
                    "JoinedMethod": acct.joined_method,
                    "JoinedTimestamp": acct.joined_timestamp.timestamp() as f64,
                    "DelegationEnabledDate": admin.registered_at.timestamp() as f64,
                }))
            })
            .collect();
        let (page, token) = paginate_checked(&entries, next_token.as_deref(), max_results)
            .map_err(|_| invalid_input("Invalid NextToken"))?;
        let mut body = json!({ "DelegatedAdministrators": page });
        if let Some(t) = token {
            body["NextToken"] = json!(t);
        }
        Ok(AwsResponse::ok_json(body))
    }
}
