//! `OrganizationsService` `tags` family — extracted from service.rs by audit-2026-05-19.

use super::*;

impl OrganizationsService {
    pub(super) fn tag_resource(&self, req: &AwsRequest) -> Result<AwsResponse, AwsServiceError> {
        let body = req.json_body();
        let resource_id = required_str(&body, "ResourceId")?.to_string();
        let tags = parse_tags(body.get("Tags"));
        let mut guard = self.state.write();
        let org = self.management_org_mut(&mut guard, &req.account_id)?;
        org.set_resource_tags(&resource_id, &tags);
        Ok(AwsResponse::ok_json(json!({})))
    }

    pub(super) fn untag_resource(&self, req: &AwsRequest) -> Result<AwsResponse, AwsServiceError> {
        let body = req.json_body();
        let resource_id = required_str(&body, "ResourceId")?.to_string();
        let tag_keys: Vec<String> = body
            .get("TagKeys")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        let mut guard = self.state.write();
        let org = self.management_org_mut(&mut guard, &req.account_id)?;
        org.untag_resource(&resource_id, &tag_keys);
        Ok(AwsResponse::ok_json(json!({})))
    }

    pub(super) fn list_tags_for_resource(
        &self,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        let body = req.json_body();
        let resource_id = required_str(&body, "ResourceId")?.to_string();
        let guard = self.state.read();
        let org = self.require_member(&guard, &req.account_id)?;
        let tags: Vec<Value> = org
            .list_resource_tags(&resource_id)
            .into_iter()
            .map(|(k, v)| json!({"Key": k, "Value": v}))
            .collect();
        Ok(AwsResponse::ok_json(json!({ "Tags": tags })))
    }
}
