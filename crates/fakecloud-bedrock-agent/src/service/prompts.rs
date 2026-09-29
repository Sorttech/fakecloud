//! `BedrockAgentService` `prompts` family — extracted from service.rs by audit-2026-05-19.

use super::*;

/// Split a `promptIdentifier` into the prompt ID and the version it pins, if
/// any. The identifier is the bare prompt ID or the prompt ARN, optionally
/// suffixed `:<version>` (`arn:...:prompt/<id>[:<version>]`).
fn prompt_id_of(identifier: &str) -> (String, Option<String>) {
    let identifier = decode_label(identifier);
    match identifier.rsplit_once(":prompt/") {
        Some((_, rest)) => match rest.split_once(':') {
            Some((id, version)) => (id.to_string(), Some(version.to_string())),
            None => (rest.to_string(), None),
        },
        None => (identifier, None),
    }
}

/// The prompt ID and the version the request targets: `promptVersion` (an
/// HTTP query member) wins over a version pinned in the identifier ARN.
fn prompt_target(
    req: &AwsRequest,
    body: &Value,
) -> Result<(String, Option<String>), AwsServiceError> {
    let (id, pinned) = prompt_id_of(&req_str(body, "promptIdentifier")?);
    let version = opt_str(body, "promptVersion")
        .or_else(|| req.query_params.get("promptVersion").cloned())
        .or(pinned)
        // DRAFT names the working draft, the same as naming no version.
        .filter(|v| v != "DRAFT");
    Ok((id, version))
}

fn prompt_not_found(id: &str) -> AwsServiceError {
    not_found(format!("Prompt {id} not found"))
}

fn prompt_version_not_found(version: &str) -> AwsServiceError {
    not_found(format!("Prompt version {version} not found"))
}

impl BedrockAgentService {
    pub(super) fn create_prompt(&self, req: &AwsRequest) -> Result<AwsResponse, AwsServiceError> {
        let body = req.json_body();
        let name = req_str(&body, "name")?;
        let id = short_id();
        let now_dt = now();
        let prompt = Prompt {
            prompt_id: id.clone(),
            name,
            description: opt_str(&body, "description"),
            variants: opt_array(&body, "variants"),
            version: "DRAFT".to_string(),
            created_at: now_dt,
            updated_at: now_dt,
            arn: prompt_arn(&req.region, &req.account_id, &id),
            customer_encryption_key_arn: opt_str(&body, "customerEncryptionKeyArn"),
            default_variant: opt_str(&body, "defaultVariant"),
        };
        let out = prompt_json(&prompt);
        let mut accts = self.state.write();
        let state = accts.get_or_create(&req.account_id, &req.region);
        state.prompts.insert(id, prompt);
        Ok(AwsResponse::json_value(StatusCode::CREATED, out))
    }

    pub(super) fn get_prompt(&self, req: &AwsRequest) -> Result<AwsResponse, AwsServiceError> {
        let body = req.json_body();
        let (id, version) = prompt_target(req, &body)?;
        let accts = self.state.read();
        let state = accts
            .get(&req.account_id)
            .ok_or_else(|| prompt_not_found(&id))?;
        let p = state
            .prompts
            .get(&id)
            .ok_or_else(|| prompt_not_found(&id))?;
        let out = match version {
            None => prompt_json(p),
            Some(version) => {
                let v = state
                    .prompt_versions
                    .get(&id)
                    .and_then(|vs| vs.iter().find(|v| v.prompt_version == version))
                    .ok_or_else(|| prompt_version_not_found(&version))?;
                prompt_version_json(p, v)
            }
        };
        Ok(AwsResponse::ok_json(out))
    }

    pub(super) fn create_prompt_version(
        &self,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        // The routing layer surfaces the prompt identifier (path segment) into
        // the body under `promptIdentifier`, so we read it back here. The
        // resulting version is numbered incrementally per the Smithy contract.
        let body = req.json_body();
        let (id, _) = prompt_id_of(&req_str(&body, "promptIdentifier")?);
        let now_dt = now();
        let mut accts = self.state.write();
        let state = accts.get_or_create(&req.account_id, &req.region);
        let prompt = state
            .prompts
            .get(&id)
            .ok_or_else(|| prompt_not_found(&id))?
            .clone();
        let versions = state.prompt_versions.entry(id.clone()).or_default();
        let version_num = (versions.len() as u64 + 1).to_string();
        let pv = PromptVersion {
            prompt_version: version_num,
            prompt_id: id,
            description: opt_str(&body, "description").or(prompt.description.clone()),
            created_at: now_dt,
            updated_at: now_dt,
            variants: prompt.variants.clone(),
            name: Some(prompt.name.clone()),
            default_variant: prompt.default_variant.clone(),
        };
        let out = prompt_version_json(&prompt, &pv);
        versions.push(pv);
        Ok(AwsResponse::json_value(StatusCode::CREATED, out))
    }

    pub(super) fn list_prompts(&self, req: &AwsRequest) -> Result<AwsResponse, AwsServiceError> {
        let accts = self.state.read();
        let list: Vec<Value> = accts
            .get(&req.account_id)
            .map(|s| s.prompts.values().map(prompt_summary_json).collect())
            .unwrap_or_default();
        Ok(AwsResponse::ok_json(json!({ "promptSummaries": list })))
    }

    pub(super) fn update_prompt(&self, req: &AwsRequest) -> Result<AwsResponse, AwsServiceError> {
        let body = req.json_body();
        let (id, _) = prompt_id_of(&req_str(&body, "promptIdentifier")?);
        let mut accts = self.state.write();
        let state = accts.get_or_create(&req.account_id, &req.region);
        let p = state
            .prompts
            .get_mut(&id)
            .ok_or_else(|| prompt_not_found(&id))?;
        p.updated_at = now();
        if let Some(n) = opt_str(&body, "name") {
            p.name = n;
        }
        if let Some(d) = opt_str(&body, "description") {
            p.description = Some(d);
        }
        if body.get("variants").is_some() {
            p.variants = opt_array(&body, "variants");
        }
        if let Some(k) = opt_str(&body, "customerEncryptionKeyArn") {
            p.customer_encryption_key_arn = Some(k);
        }
        if let Some(dv) = opt_str(&body, "defaultVariant") {
            p.default_variant = Some(dv);
        }
        Ok(AwsResponse::ok_json(prompt_json(p)))
    }

    pub(super) fn delete_prompt(&self, req: &AwsRequest) -> Result<AwsResponse, AwsServiceError> {
        let body = req.json_body();
        let (id, version) = prompt_target(req, &body)?;
        let mut accts = self.state.write();
        let state = accts.get_or_create(&req.account_id, &req.region);
        if !state.prompts.contains_key(&id) {
            return Err(prompt_not_found(&id));
        }
        match version {
            // Naming a version deletes just that version.
            Some(version) => {
                let versions = state
                    .prompt_versions
                    .get_mut(&id)
                    .ok_or_else(|| prompt_version_not_found(&version))?;
                let pos = versions
                    .iter()
                    .position(|v| v.prompt_version == version)
                    .ok_or_else(|| prompt_version_not_found(&version))?;
                versions.remove(pos);
                Ok(AwsResponse::ok_json(json!({
                    "id": id,
                    "version": version,
                })))
            }
            None => {
                state.prompts.remove(&id);
                state.prompt_versions.remove(&id);
                Ok(AwsResponse::ok_json(json!({ "id": id })))
            }
        }
    }

    pub(super) fn list_prompt_versions(
        &self,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        let body = req.json_body();
        let (id, _) = prompt_id_of(&req_str(&body, "promptIdentifier")?);
        let accts = self.state.read();
        let state = accts
            .get(&req.account_id)
            .ok_or_else(|| prompt_not_found(&id))?;
        let prompt_arn = state.prompts.get(&id).map(|p| p.arn.as_str());
        let versions: Vec<Value> = prompt_arn
            .zip(state.prompt_versions.get(&id))
            .map(|(prompt_arn, vs)| {
                vs.iter()
                    .map(|v| {
                        let mut o = json!({
                            "id": v.prompt_id,
                            "version": v.prompt_version,
                            "arn": format!("{prompt_arn}:{}", v.prompt_version),
                            "createdAt": v.created_at.to_rfc3339(),
                            "updatedAt": v.updated_at.to_rfc3339(),
                        });
                        if let Some(ref d) = v.description {
                            o["description"] = json!(d);
                        }
                        o
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(AwsResponse::ok_json(json!({ "promptSummaries": versions })))
    }

    pub(super) fn get_prompt_version(
        &self,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        let body = req.json_body();
        let (id, _) = prompt_id_of(&req_str(&body, "promptIdentifier")?);
        let version = req_str(&body, "promptVersion")?;
        let accts = self.state.read();
        let state = accts
            .get(&req.account_id)
            .ok_or_else(|| prompt_not_found(&id))?;
        let prompt = state
            .prompts
            .get(&id)
            .ok_or_else(|| prompt_not_found(&id))?;
        let v = state
            .prompt_versions
            .get(&id)
            .and_then(|vs| vs.iter().find(|v| v.prompt_version == version))
            .ok_or_else(|| prompt_version_not_found(&version))?;
        Ok(AwsResponse::ok_json(prompt_version_json(prompt, v)))
    }
}
