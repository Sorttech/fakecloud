//! `BedrockAgentService` `prompts` family — extracted from service.rs by audit-2026-05-19.

use super::*;

/// Split a `promptIdentifier` into the prompt ID and the version it pins, if
/// any. The identifier is the bare prompt ID, or the ARN of a prompt in the
/// caller's account and region, optionally suffixed `:<version>`
/// (`arn:...:prompt/<id>[:<version>]`). An ARN of anything else names no
/// prompt here.
fn prompt_id_of(
    req: &AwsRequest,
    identifier: &str,
) -> Result<(String, Option<String>), AwsServiceError> {
    match parse_identifier(req, identifier) {
        Some(Identifier::Id(id)) => Ok((id.to_string(), None)),
        Some(Identifier::Resource(resource)) => {
            let rest = resource
                .strip_prefix("prompt/")
                .filter(|rest| !rest.is_empty() && !rest.contains('/'))
                .ok_or_else(|| prompt_not_found(identifier))?;
            Ok(match rest.split_once(':') {
                Some((id, version)) => (id.to_string(), Some(version.to_string())),
                None => (rest.to_string(), None),
            })
        }
        None => Err(prompt_not_found(identifier)),
    }
}

/// The version a request's `promptVersion` (an `@httpQuery` member) names, or
/// failing that the version pinned in the identifier ARN.
fn requested_version(req: &AwsRequest, body: &Value, pinned: Option<String>) -> Option<String> {
    opt_str(body, "promptVersion")
        .or_else(|| req.query_params.get("promptVersion").cloned())
        .or(pinned)
}

fn prompt_not_found(id: &str) -> AwsServiceError {
    not_found(format!("Prompt {id} not found"))
}

fn prompt_version_not_found(version: &str) -> AwsServiceError {
    not_found(format!("Prompt version {version} not found"))
}

/// `PromptSummary` for numbered version `v` of prompt `p`.
fn prompt_version_summary_json(p: &Prompt, v: &PromptVersion) -> Value {
    let mut o = json!({
        "name": v.name.as_deref().unwrap_or(&p.name),
        "id": p.prompt_id,
        "arn": format!("{}:{}", p.arn, v.prompt_version),
        "version": v.prompt_version,
        "createdAt": v.created_at.to_rfc3339(),
        "updatedAt": v.updated_at.to_rfc3339(),
    });
    if let Some(ref d) = v.description {
        o["description"] = json!(d);
    }
    o
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
            latest_version: 0,
        };
        let out = prompt_json(&prompt);
        let mut accts = self.state.write();
        let state = accts.get_or_create(&req.account_id, &req.region);
        state.prompts.insert(id, prompt);
        Ok(AwsResponse::json_value(StatusCode::CREATED, out))
    }

    pub(super) fn get_prompt(&self, req: &AwsRequest) -> Result<AwsResponse, AwsServiceError> {
        let body = req.json_body();
        let (id, pinned) = prompt_id_of(req, &req_str(&body, "promptIdentifier")?)?;
        // GetPrompt's `promptVersion` is a `Version`: `DRAFT` names the
        // working draft, the same as naming no version.
        let version = requested_version(req, &body, pinned).filter(|v| v != "DRAFT");
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
        // the body under `promptIdentifier`, so we read it back here.
        let body = req.json_body();
        let (id, _) = prompt_id_of(req, &req_str(&body, "promptIdentifier")?)?;
        let now_dt = now();
        let mut accts = self.state.write();
        let state = accts.get_or_create(&req.account_id, &req.region);
        // `prompts` and `prompt_versions` are disjoint fields, so the prompt
        // can be borrowed while its version list is extended.
        let prompt = state
            .prompts
            .get_mut(&id)
            .ok_or_else(|| prompt_not_found(&id))?;
        let versions = state.prompt_versions.entry(id.clone()).or_default();
        let version_num = next_version(
            &mut prompt.latest_version,
            versions.iter().map(|v| v.prompt_version.as_str()),
        );
        let pv = PromptVersion {
            prompt_version: version_num,
            prompt_id: id,
            description: opt_str(&body, "description").or_else(|| prompt.description.clone()),
            created_at: now_dt,
            updated_at: now_dt,
            variants: prompt.variants.clone(),
            name: Some(prompt.name.clone()),
            default_variant: prompt.default_variant.clone(),
            customer_encryption_key_arn: prompt.customer_encryption_key_arn.clone(),
        };
        let out = prompt_version_json(prompt, &pv);
        versions.push(pv);
        Ok(AwsResponse::json_value(StatusCode::CREATED, out))
    }

    /// Without `promptIdentifier`, the working draft of every prompt; with
    /// it, every version of that one prompt (the draft, then each numbered
    /// version).
    pub(super) fn list_prompts(&self, req: &AwsRequest) -> Result<AwsResponse, AwsServiceError> {
        let body = req.json_body();
        let identifier = opt_str(&body, "promptIdentifier")
            .or_else(|| req.query_params.get("promptIdentifier").cloned());
        let accts = self.state.read();
        let state = accts.get(&req.account_id);
        let list: Vec<Value> = match identifier {
            None => state
                .map(|s| s.prompts.values().map(prompt_summary_json).collect())
                .unwrap_or_default(),
            Some(identifier) => {
                let (id, _) = prompt_id_of(req, &identifier)?;
                let state = state.ok_or_else(|| prompt_not_found(&id))?;
                let p = state
                    .prompts
                    .get(&id)
                    .ok_or_else(|| prompt_not_found(&id))?;
                std::iter::once(prompt_summary_json(p))
                    .chain(
                        state
                            .prompt_versions
                            .get(&id)
                            .into_iter()
                            .flatten()
                            .map(|v| prompt_version_summary_json(p, v)),
                    )
                    .collect()
            }
        };
        Ok(AwsResponse::ok_json(json!({ "promptSummaries": list })))
    }

    pub(super) fn update_prompt(&self, req: &AwsRequest) -> Result<AwsResponse, AwsServiceError> {
        let body = req.json_body();
        let (id, _) = prompt_id_of(req, &req_str(&body, "promptIdentifier")?)?;
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
        let (id, pinned) = prompt_id_of(req, &req_str(&body, "promptIdentifier")?)?;
        let version = requested_version(req, &body, pinned);
        // DeletePrompt's `promptVersion` is a `NumericalVersion`
        // (`^[0-9]{1,5}$`): only a numbered version can be deleted on its own,
        // and `DRAFT` is not a way to name the whole prompt.
        if let Some(ref v) = version {
            if v.is_empty() || v.len() > 5 || !v.bytes().all(|b| b.is_ascii_digit()) {
                return Err(AwsServiceError::aws_error(
                    StatusCode::BAD_REQUEST,
                    "ValidationException",
                    format!(
                        "1 validation error detected: Value '{v}' at 'promptVersion' failed to \
                         satisfy constraint: Member must satisfy regular expression pattern: \
                         ^[0-9]{{1,5}}$"
                    ),
                ));
            }
        }
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
}
