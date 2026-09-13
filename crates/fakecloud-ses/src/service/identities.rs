use chrono::Utc;
use http::StatusCode;
use serde_json::{json, Value};

use fakecloud_core::service::{AwsRequest, AwsResponse, AwsServiceError};

use crate::state::EmailIdentity;
use crate::state::IdentityCertificate;
use crate::state::SesState;

use super::SesV2Service;

/// Expected DNS records the customer must publish for SES to accept the
/// configured mail-from domain. Mirrors the JSON shape SES returns in
/// `GetEmailIdentity.MailFromAttributes.MailFromDomainDnsRecords`.
pub(crate) fn mail_from_dns_records(domain: &str, region: &str) -> Vec<Value> {
    if domain.is_empty() {
        return Vec::new();
    }
    vec![
        json!({
            "Name": domain,
            "Type": "MX",
            "Value": format!("10 feedback-smtp.{region}.amazonses.com"),
        }),
        json!({
            "Name": domain,
            "Type": "TXT",
            "Value": "\"v=spf1 include:amazonses.com ~all\"",
        }),
    ]
}

impl SesV2Service {
    pub(super) fn create_email_identity(
        &self,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        let body: Value = Self::parse_body(req)?;
        let identity_name = match body["EmailIdentity"].as_str() {
            Some(name) => name.to_string(),
            None => {
                return Ok(Self::json_error(
                    StatusCode::BAD_REQUEST,
                    "BadRequestException",
                    "EmailIdentity is required",
                ));
            }
        };
        if identity_name.is_empty() {
            return Ok(Self::json_error(
                StatusCode::BAD_REQUEST,
                "BadRequestException",
                "EmailIdentity must not be empty",
            ));
        }

        let mut accounts = self.state.write();
        let state = accounts.get_or_create(&req.account_id);

        if state.identities.contains_key(&identity_name) {
            return Ok(Self::json_error(
                StatusCode::CONFLICT,
                "AlreadyExistsException",
                &format!("Identity {} already exists", identity_name),
            ));
        }

        let identity_type = if identity_name.contains('@') {
            "EMAIL_ADDRESS"
        } else {
            "DOMAIN"
        };

        // Honor the optional `ConfigurationSetName` from the input — the
        // Smithy model round-trips it on GetEmailIdentity, so dropping it
        // here is a real input-drop bug.
        let configuration_set_name = body["ConfigurationSetName"].as_str().map(|s| s.to_string());

        // DKIM configuration. CreateEmailIdentity's DkimSigningAttributes
        // selects between Easy DKIM (AWS-managed keypair, optionally with a
        // requested NextSigningKeyLength) and BYODKIM (the caller supplies
        // their own selector + private key, origin EXTERNAL). Previously
        // this was hardcoded to Easy DKIM, silently dropping BYODKIM.
        let dkim = &body["DkimSigningAttributes"];
        let byo_selector = dkim["DomainSigningSelector"].as_str();
        let byo_private_key = dkim["DomainSigningPrivateKey"].as_str();
        let (dkim_origin, dkim_private_key, dkim_selector, dkim_key_length, dkim_public_key) =
            if let (Some(selector), Some(private_key)) = (byo_selector, byo_private_key) {
                // BYODKIM: honor the caller's selector + private key. AWS
                // supplies the key as base64-encoded DER, so normalize it to
                // the PEM form the signer expects before storing. There is no
                // AWS-generated public key in this mode.
                (
                    "EXTERNAL".to_string(),
                    Some(crate::dkim::normalize_byodkim_private_key(private_key)),
                    Some(selector.to_string()),
                    None,
                    None,
                )
            } else {
                // Easy DKIM: auto-provision a fresh keypair so SendEmail can
                // stamp DKIM-Signature headers without a follow-up
                // PutEmailIdentityDkimSigningAttributes. NextSigningKeyLength
                // (if supplied) selects the reported key length.
                let key_length = dkim["NextSigningKeyLength"]
                    .as_str()
                    .unwrap_or("RSA_2048_BIT")
                    .to_string();
                let (priv_pem, pub_b64) = crate::dkim::generate_easy_dkim_keypair();
                (
                    "AWS_SES".to_string(),
                    Some(priv_pem),
                    Some("fakecloudses".to_string()),
                    Some(key_length),
                    Some(pub_b64),
                )
            };
        let dkim_origin_resp = dkim_origin.clone();
        let dkim_selector_resp = dkim_selector.clone();
        let dkim_key_length_resp = dkim_key_length.clone();
        let identity = EmailIdentity {
            identity_name: identity_name.clone(),
            identity_type: identity_type.to_string(),
            verified: true,
            created_at: Utc::now(),
            dkim_signing_enabled: true,
            dkim_signing_attributes_origin: dkim_origin,
            dkim_domain_signing_private_key: dkim_private_key,
            dkim_domain_signing_selector: dkim_selector,
            dkim_next_signing_key_length: dkim_key_length,
            dkim_public_key_b64: dkim_public_key,
            email_forwarding_enabled: true,
            mail_from_domain: None,
            mail_from_behavior_on_mx_failure: "USE_DEFAULT_VALUE".to_string(),
            mail_from_domain_status: "NotStarted".to_string(),
            configuration_set_name,
            bounce_topic: None,
            complaint_topic: None,
            delivery_topic: None,
            verification_token: None,
        };

        state.identities.insert(identity_name.clone(), identity);

        // Persist Tags via the per-ARN tag map TagResource/ListTagsForResource
        // use, so the round-trip echo for Tags is honored end-to-end.
        // Replace rather than merge so a Create after Delete (or a
        // Create that omits Tags) doesn't inherit stale entries from a
        // previous incarnation of the ARN.
        let arn = format!(
            "arn:aws:ses:{}:{}:identity/{}",
            req.region, req.account_id, identity_name
        );
        if let Some(tags_arr) = body["Tags"].as_array() {
            let mut tag_map = std::collections::BTreeMap::new();
            for tag in tags_arr {
                if let (Some(k), Some(v)) = (tag["Key"].as_str(), tag["Value"].as_str()) {
                    tag_map.insert(k.to_string(), v.to_string());
                }
            }
            state.tags.insert(arn, tag_map);
        } else {
            state.tags.remove(&arn);
        }

        let mut dkim_attrs = json!({
            "SigningEnabled": true,
            "Status": "SUCCESS",
            "SigningAttributesOrigin": dkim_origin_resp,
            "Tokens": [
                "token1",
                "token2",
                "token3",
            ],
        });
        if let Some(ref selector) = dkim_selector_resp {
            dkim_attrs["DomainSigningSelector"] = json!(selector);
        }
        if let Some(ref len) = dkim_key_length_resp {
            dkim_attrs["CurrentSigningKeyLength"] = json!(len);
            dkim_attrs["NextSigningKeyLength"] = json!(len);
        }
        let response = json!({
            "IdentityType": identity_type,
            "VerifiedForSendingStatus": true,
            "DkimAttributes": dkim_attrs,
        });

        Ok(AwsResponse::json(StatusCode::OK, response.to_string()))
    }

    pub(super) fn list_email_identities(
        &self,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        let accounts = self.state.read();
        let empty = SesState::new(&req.account_id, &req.region);
        let state = accounts.get(&req.account_id).unwrap_or(&empty);
        let identities: Vec<Value> = state
            .identities
            .values()
            .map(|id| {
                json!({
                    "IdentityType": id.identity_type,
                    "IdentityName": id.identity_name,
                    "SendingEnabled": true,
                })
            })
            .collect();

        let response = json!({
            "EmailIdentities": identities,
        });

        Ok(AwsResponse::json(StatusCode::OK, response.to_string()))
    }

    pub(super) fn get_email_identity(
        &self,
        identity_name: &str,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        let mut accounts = self.state.write();
        let state = accounts.get_or_create(&req.account_id);
        // Use the request region (not the frozen state.region) so the
        // MAIL FROM MX host matches this response's identity ARN below,
        // which is also derived from req.region.
        let region = req.region.clone();
        let identity = match state.identities.get_mut(identity_name) {
            Some(id) => id,
            None => {
                return Ok(Self::json_error(
                    StatusCode::NOT_FOUND,
                    "NotFoundException",
                    &format!("Identity {} does not exist", identity_name),
                ));
            }
        };

        let mail_from_domain = identity.mail_from_domain.clone().unwrap_or_default();
        // Auto-advance Pending -> Success on next read; matches real SES once
        // it observes the expected MX/TXT records on the configured mail-from
        // domain. Admin endpoint flips back to Failed for tests.
        if identity.mail_from_domain_status == "Pending" && !mail_from_domain.is_empty() {
            identity.mail_from_domain_status = "Success".to_string();
        }
        if mail_from_domain.is_empty() {
            identity.mail_from_domain_status = "NotStarted".to_string();
        }
        let mail_from_status = identity.mail_from_domain_status.clone();
        let behavior = identity.mail_from_behavior_on_mx_failure.clone();
        let mail_from_dns = mail_from_dns_records(&mail_from_domain, &region);

        let mut dkim_attrs = json!({
            "SigningEnabled": identity.dkim_signing_enabled,
            "Status": "SUCCESS",
            "SigningAttributesOrigin": identity.dkim_signing_attributes_origin,
            "Tokens": ["token1", "token2", "token3"],
        });
        if let Some(ref selector) = identity.dkim_domain_signing_selector {
            dkim_attrs["LastKeyGenerationTimestamp"] = json!(identity.created_at.timestamp());
            dkim_attrs["CurrentSigningKeyLength"] = json!(identity
                .dkim_next_signing_key_length
                .as_deref()
                .unwrap_or("RSA_2048_BIT"));
            dkim_attrs["NextSigningKeyLength"] = json!(identity
                .dkim_next_signing_key_length
                .as_deref()
                .unwrap_or("RSA_2048_BIT"));
            dkim_attrs["DomainSigningSelector"] = json!(selector);
        }
        let mut response = json!({
            "IdentityType": identity.identity_type,
            "VerifiedForSendingStatus": true,
            "FeedbackForwardingStatus": identity.email_forwarding_enabled,
            "DkimAttributes": dkim_attrs,
            "MailFromAttributes": {
                "MailFromDomain": mail_from_domain,
                "MailFromDomainStatus": mail_from_status,
                "BehaviorOnMxFailure": behavior,
            },
            "Tags": [],
        });
        if !mail_from_dns.is_empty() {
            response["MailFromAttributes"]["MailFromDomainDnsRecords"] = json!(mail_from_dns);
        }

        let configuration_set_name = identity.configuration_set_name.clone();
        // Release the &mut on `identity` before borrowing `state.tags`.
        let _ = identity;
        if let Some(cs) = configuration_set_name {
            response["ConfigurationSetName"] = json!(cs);
        }

        // Surface stored tags from the per-ARN tag map. Echoing keeps
        // the round-trip honest: anything CreateEmailIdentity/TagResource
        // wrote is visible on the read side.
        let arn = format!(
            "arn:aws:ses:{}:{}:identity/{}",
            req.region, req.account_id, identity_name
        );
        if let Some(tag_map) = state.tags.get(&arn) {
            response["Tags"] =
                Value::Array(fakecloud_core::tags::tags_to_json(tag_map, "Key", "Value"));
        }

        Ok(AwsResponse::json(StatusCode::OK, response.to_string()))
    }

    pub(super) fn delete_email_identity(
        &self,
        identity_name: &str,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        let mut accounts = self.state.write();
        let state = accounts.get_or_create(&req.account_id);

        if state.identities.remove(identity_name).is_none() {
            return Ok(Self::json_error(
                StatusCode::NOT_FOUND,
                "NotFoundException",
                &format!("Identity {} does not exist", identity_name),
            ));
        }

        // Remove tags for this identity
        let arn = format!(
            "arn:aws:ses:{}:{}:identity/{}",
            req.region, req.account_id, identity_name
        );
        state.tags.remove(&arn);

        // Remove policies for this identity
        state.identity_policies.remove(identity_name);

        // Remove S/MIME certificate associations for this identity
        state.identity_certificates.remove(identity_name);

        Ok(AwsResponse::json(StatusCode::OK, "{}"))
    }

    // --- Email Identity Policy operations ---

    pub(super) fn create_email_identity_policy(
        &self,
        identity_name: &str,
        policy_name: &str,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        Self::require_nonempty("EmailIdentity", identity_name)?;
        Self::require_nonempty("PolicyName", policy_name)?;
        // Reject unsubstituted URI template placeholders (e.g. SDK fed
        // an empty or absent PolicyName and the literal "{PolicyName}"
        // remained in the URL).
        if policy_name.starts_with('{') && policy_name.ends_with('}') {
            return Err(AwsServiceError::aws_error(
                StatusCode::BAD_REQUEST,
                "BadRequestException",
                "PolicyName is required",
            ));
        }
        if policy_name.is_empty() || policy_name.len() > 64 {
            return Err(AwsServiceError::aws_error(
                StatusCode::BAD_REQUEST,
                "BadRequestException",
                "PolicyName length must be between 1 and 64",
            ));
        }
        // Smithy regex: `^[a-zA-Z0-9_-]+$`
        if !policy_name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            return Err(AwsServiceError::aws_error(
                StatusCode::BAD_REQUEST,
                "BadRequestException",
                "PolicyName must match pattern [a-zA-Z0-9_-]+",
            ));
        }
        let body: Value = Self::parse_body(req)?;

        let policy = match body["Policy"].as_str() {
            Some(p) => p.to_string(),
            None => {
                return Ok(Self::json_error(
                    StatusCode::BAD_REQUEST,
                    "BadRequestException",
                    "Policy is required",
                ));
            }
        };

        let mut accounts = self.state.write();
        let state = accounts.get_or_create(&req.account_id);

        if !state.identities.contains_key(identity_name) {
            return Ok(Self::json_error(
                StatusCode::NOT_FOUND,
                "NotFoundException",
                &format!("Identity {} does not exist", identity_name),
            ));
        }

        let policies = state
            .identity_policies
            .entry(identity_name.to_string())
            .or_default();

        if policies.contains_key(policy_name) {
            return Ok(Self::json_error(
                StatusCode::CONFLICT,
                "AlreadyExistsException",
                &format!("Policy {} already exists", policy_name),
            ));
        }

        policies.insert(policy_name.to_string(), policy);

        Ok(AwsResponse::json(StatusCode::OK, "{}"))
    }

    pub(super) fn get_email_identity_policies(
        &self,
        identity_name: &str,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        let accounts = self.state.read();
        let empty = SesState::new(&req.account_id, &req.region);
        let state = accounts.get(&req.account_id).unwrap_or(&empty);

        if !state.identities.contains_key(identity_name) {
            return Ok(Self::json_error(
                StatusCode::NOT_FOUND,
                "NotFoundException",
                &format!("Identity {} does not exist", identity_name),
            ));
        }

        let policies = state
            .identity_policies
            .get(identity_name)
            .cloned()
            .unwrap_or_default();

        let policies_json: Value = policies
            .into_iter()
            .map(|(k, v)| (k, Value::String(v)))
            .collect::<serde_json::Map<String, Value>>()
            .into();

        let response = json!({
            "Policies": policies_json,
        });

        Ok(AwsResponse::json(StatusCode::OK, response.to_string()))
    }

    pub(super) fn update_email_identity_policy(
        &self,
        identity_name: &str,
        policy_name: &str,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        let body: Value = Self::parse_body(req)?;

        let policy = match body["Policy"].as_str() {
            Some(p) => p.to_string(),
            None => {
                return Ok(Self::json_error(
                    StatusCode::BAD_REQUEST,
                    "BadRequestException",
                    "Policy is required",
                ));
            }
        };

        let mut accounts = self.state.write();
        let state = accounts.get_or_create(&req.account_id);

        if !state.identities.contains_key(identity_name) {
            return Ok(Self::json_error(
                StatusCode::NOT_FOUND,
                "NotFoundException",
                &format!("Identity {} does not exist", identity_name),
            ));
        }

        let policies = state
            .identity_policies
            .entry(identity_name.to_string())
            .or_default();

        if !policies.contains_key(policy_name) {
            return Ok(Self::json_error(
                StatusCode::NOT_FOUND,
                "NotFoundException",
                &format!("Policy {} does not exist", policy_name),
            ));
        }

        policies.insert(policy_name.to_string(), policy);

        Ok(AwsResponse::json(StatusCode::OK, "{}"))
    }

    pub(super) fn delete_email_identity_policy(
        &self,
        identity_name: &str,
        policy_name: &str,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        let mut accounts = self.state.write();
        let state = accounts.get_or_create(&req.account_id);

        if !state.identities.contains_key(identity_name) {
            return Ok(Self::json_error(
                StatusCode::NOT_FOUND,
                "NotFoundException",
                &format!("Identity {} does not exist", identity_name),
            ));
        }

        let policies = state
            .identity_policies
            .entry(identity_name.to_string())
            .or_default();

        if policies.remove(policy_name).is_none() {
            return Ok(Self::json_error(
                StatusCode::NOT_FOUND,
                "NotFoundException",
                &format!("Policy {} does not exist", policy_name),
            ));
        }

        Ok(AwsResponse::json(StatusCode::OK, "{}"))
    }

    // --- Identity Attribute operations ---

    pub(super) fn put_email_identity_dkim_attributes(
        &self,
        identity_name: &str,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        let body: Value = Self::parse_body(req)?;
        let mut accounts = self.state.write();
        let state = accounts.get_or_create(&req.account_id);

        let identity = match state.identities.get_mut(identity_name) {
            Some(id) => id,
            None => {
                return Ok(Self::json_error(
                    StatusCode::NOT_FOUND,
                    "NotFoundException",
                    &format!("Identity {} does not exist", identity_name),
                ));
            }
        };

        if let Some(enabled) = body["SigningEnabled"].as_bool() {
            identity.dkim_signing_enabled = enabled;
        }
        // Lazily provision an Easy DKIM keypair the moment signing is
        // enabled if no caller-supplied key is on file. Mirrors how real
        // SES auto-generates the per-identity keypair on enable so the
        // next SendEmail can stamp a real DKIM-Signature.
        if identity.dkim_signing_enabled && identity.dkim_domain_signing_private_key.is_none() {
            let (priv_pem, pub_b64) = crate::dkim::generate_easy_dkim_keypair();
            identity.dkim_domain_signing_private_key = Some(priv_pem);
            identity.dkim_public_key_b64 = Some(pub_b64);
            if identity.dkim_domain_signing_selector.is_none() {
                identity.dkim_domain_signing_selector = Some("fakecloudses".to_string());
            }
            if identity.dkim_next_signing_key_length.is_none() {
                identity.dkim_next_signing_key_length = Some("RSA_2048_BIT".to_string());
            }
        }

        Ok(AwsResponse::json(StatusCode::OK, "{}"))
    }

    pub(super) fn put_email_identity_dkim_signing_attributes(
        &self,
        identity_name: &str,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        let body: Value = Self::parse_body(req)?;
        let mut accounts = self.state.write();
        let state = accounts.get_or_create(&req.account_id);

        let identity = match state.identities.get_mut(identity_name) {
            Some(id) => id,
            None => {
                return Ok(Self::json_error(
                    StatusCode::NOT_FOUND,
                    "NotFoundException",
                    &format!("Identity {} does not exist", identity_name),
                ));
            }
        };

        let origin = body["SigningAttributesOrigin"]
            .as_str()
            .unwrap_or(&identity.dkim_signing_attributes_origin)
            .to_string();
        identity.dkim_signing_attributes_origin = origin.clone();

        if let Some(attrs) = body.get("SigningAttributes") {
            if let Some(key) = attrs["DomainSigningPrivateKey"].as_str() {
                // AWS supplies BYODKIM keys as base64-encoded DER; normalize to
                // the PEM form the signer expects so signing actually works.
                identity.dkim_domain_signing_private_key =
                    Some(crate::dkim::normalize_byodkim_private_key(key));
                identity.dkim_public_key_b64 = None;
            }
            if let Some(selector) = attrs["DomainSigningSelector"].as_str() {
                identity.dkim_domain_signing_selector = Some(selector.to_string());
            }
            if let Some(length) = attrs["NextSigningKeyLength"].as_str() {
                identity.dkim_next_signing_key_length = Some(length.to_string());
            }
        }

        // Easy DKIM: AWS_SES origin without a caller-supplied key triggers
        // generation of a fresh RSA-2048 keypair. The public half is what
        // SES would publish via the `*.dkim.amazonses.com` CNAME chain.
        if origin == "AWS_SES" && identity.dkim_domain_signing_private_key.is_none() {
            let (priv_pem, pub_b64) = crate::dkim::generate_easy_dkim_keypair();
            identity.dkim_domain_signing_private_key = Some(priv_pem);
            identity.dkim_public_key_b64 = Some(pub_b64);
            if identity.dkim_domain_signing_selector.is_none() {
                identity.dkim_domain_signing_selector = Some("fakecloudses".to_string());
            }
        }

        let response = json!({
            "DkimStatus": "SUCCESS",
            "DkimTokens": ["token1", "token2", "token3"],
        });

        Ok(AwsResponse::json(StatusCode::OK, response.to_string()))
    }

    pub(super) fn put_email_identity_feedback_attributes(
        &self,
        identity_name: &str,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        let body: Value = Self::parse_body(req)?;
        let mut accounts = self.state.write();
        let state = accounts.get_or_create(&req.account_id);

        let identity = match state.identities.get_mut(identity_name) {
            Some(id) => id,
            None => {
                return Ok(Self::json_error(
                    StatusCode::NOT_FOUND,
                    "NotFoundException",
                    &format!("Identity {} does not exist", identity_name),
                ));
            }
        };

        // EmailForwardingEnabled is a primitive bool: the AWS SDK omits it from
        // the request body when it is `false` (restjson drops zero-value
        // primitives), so an absent field means "disable forwarding", not "leave
        // unchanged". Default to false so PutEmailIdentityFeedbackAttributes with
        // an empty body turns forwarding off, matching the resource's default.
        identity.email_forwarding_enabled =
            body["EmailForwardingEnabled"].as_bool().unwrap_or(false);

        Ok(AwsResponse::json(StatusCode::OK, "{}"))
    }

    pub(super) fn put_email_identity_mail_from_attributes(
        &self,
        identity_name: &str,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        let body: Value = Self::parse_body(req)?;
        let mut accounts = self.state.write();
        let state = accounts.get_or_create(&req.account_id);

        let identity = match state.identities.get_mut(identity_name) {
            Some(id) => id,
            None => {
                return Ok(Self::json_error(
                    StatusCode::NOT_FOUND,
                    "NotFoundException",
                    &format!("Identity {} does not exist", identity_name),
                ));
            }
        };

        if let Some(domain) = body["MailFromDomain"].as_str() {
            let trimmed = domain.trim();
            if trimmed.is_empty() {
                identity.mail_from_domain = None;
                identity.mail_from_domain_status = "NotStarted".to_string();
            } else {
                identity.mail_from_domain = Some(trimmed.to_string());
                identity.mail_from_domain_status = "Pending".to_string();
            }
        }
        if let Some(behavior) = body["BehaviorOnMxFailure"].as_str() {
            identity.mail_from_behavior_on_mx_failure = behavior.to_string();
        }

        Ok(AwsResponse::json(StatusCode::OK, "{}"))
    }

    pub(super) fn put_email_identity_configuration_set_attributes(
        &self,
        identity_name: &str,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        let body: Value = Self::parse_body(req)?;
        let mut accounts = self.state.write();
        let state = accounts.get_or_create(&req.account_id);

        let identity = match state.identities.get_mut(identity_name) {
            Some(id) => id,
            None => {
                return Ok(Self::json_error(
                    StatusCode::NOT_FOUND,
                    "NotFoundException",
                    &format!("Identity {} does not exist", identity_name),
                ));
            }
        };

        identity.configuration_set_name =
            body["ConfigurationSetName"].as_str().map(|s| s.to_string());

        Ok(AwsResponse::json(StatusCode::OK, "{}"))
    }

    // --- S/MIME certificate associations ---

    pub(super) fn associate_email_identity_certificate(
        &self,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        let body: Value = Self::parse_body(req)?;
        let identity_name = match required_body_string(&body, "EmailIdentity") {
            Ok(name) => name,
            Err(resp) => return Ok(*resp),
        };
        let certificate_arn = match required_body_string(&body, "CertificateArn") {
            Ok(arn) => arn,
            Err(resp) => return Ok(*resp),
        };
        if let Err(msg) = validate_certificate_arn(&certificate_arn) {
            return Ok(Self::json_error(
                StatusCode::BAD_REQUEST,
                "BadRequestException",
                &msg,
            ));
        }

        let mut accounts = self.state.write();
        let state = accounts.get_or_create(&req.account_id);

        let identity = match state.identities.get(&identity_name) {
            Some(id) => id,
            None => {
                return Ok(Self::json_error(
                    StatusCode::NOT_FOUND,
                    "NotFoundException",
                    &format!("Identity {} does not exist", identity_name),
                ));
            }
        };
        let from_address = match certificate_from_address(identity, body["FromAddress"].as_str()) {
            Ok(addr) => addr,
            Err(msg) => {
                return Ok(Self::json_error(
                    StatusCode::BAD_REQUEST,
                    "BadRequestException",
                    &msg,
                ));
            }
        };

        let certificates = state
            .identity_certificates
            .entry(identity_name.clone())
            .or_default();
        // One association per from-address. Real SES rejects a second
        // association unless the existing one is on its way out
        // (DEPROVISIONING), in which case the new one replaces it.
        if let Some(existing) = certificates
            .iter_mut()
            .find(|c| c.from_address.eq_ignore_ascii_case(&from_address))
        {
            if existing.status != "DEPROVISIONING" {
                return Ok(Self::json_error(
                    StatusCode::CONFLICT,
                    "AlreadyExistsException",
                    &format!("A certificate is already associated with {from_address}"),
                ));
            }
            existing.certificate_arn = certificate_arn;
            existing.status = "PROVISIONING".to_string();
            existing.associated_at = Utc::now();
        } else {
            certificates.push(IdentityCertificate {
                from_address,
                status: "PROVISIONING".to_string(),
                certificate_arn,
                associated_at: Utc::now(),
            });
            certificates.sort_by(|a, b| a.from_address.cmp(&b.from_address));
        }

        Ok(AwsResponse::json(StatusCode::OK, "{}"))
    }

    pub(super) fn disassociate_email_identity_certificate(
        &self,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        let body: Value = Self::parse_body(req)?;
        let identity_name = match required_body_string(&body, "EmailIdentity") {
            Ok(name) => name,
            Err(resp) => return Ok(*resp),
        };

        let mut accounts = self.state.write();
        let state = accounts.get_or_create(&req.account_id);

        let identity = match state.identities.get(&identity_name) {
            Some(id) => id,
            None => {
                return Ok(Self::json_error(
                    StatusCode::NOT_FOUND,
                    "NotFoundException",
                    &format!("Identity {} does not exist", identity_name),
                ));
            }
        };
        let from_address = match certificate_from_address(identity, body["FromAddress"].as_str()) {
            Ok(addr) => addr,
            Err(msg) => {
                return Ok(Self::json_error(
                    StatusCode::BAD_REQUEST,
                    "BadRequestException",
                    &msg,
                ));
            }
        };

        // Idempotent: an identity that exists but carries no matching
        // association succeeds without changing anything. NotFoundException
        // is reserved for an unknown identity (handled above).
        let mut drained = false;
        if let Some(certificates) = state.identity_certificates.get_mut(&identity_name) {
            certificates.retain(|c| !c.from_address.eq_ignore_ascii_case(&from_address));
            drained = certificates.is_empty();
        }
        if drained {
            state.identity_certificates.remove(&identity_name);
        }

        Ok(AwsResponse::json(StatusCode::OK, "{}"))
    }

    pub(super) fn list_email_identity_certificates(
        &self,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        let body: Value = Self::parse_body(req)?;
        let identity_name = match required_body_string(&body, "EmailIdentity") {
            Ok(name) => name,
            Err(resp) => return Ok(*resp),
        };
        // NextToken / PageSize travel in the body here (the op is a POST
        // with no httpQuery bindings), unlike the GET-style listings.
        let page_size = match body.get("PageSize") {
            None | Some(Value::Null) => 20usize,
            Some(v) => match v.as_i64() {
                Some(n) if n >= 1 => n as usize,
                _ => {
                    return Ok(Self::json_error(
                        StatusCode::BAD_REQUEST,
                        "BadRequestException",
                        "PageSize must be a positive integer",
                    ));
                }
            },
        };
        let next_token = body["NextToken"].as_str().map(|s| s.to_string());

        let mut accounts = self.state.write();
        let state = accounts.get_or_create(&req.account_id);

        if !state.identities.contains_key(&identity_name) {
            return Ok(Self::json_error(
                StatusCode::NOT_FOUND,
                "NotFoundException",
                &format!("Identity {} does not exist", identity_name),
            ));
        }

        let mut page: Vec<Value> = Vec::new();
        let mut next_marker: Option<String> = None;
        if let Some(certificates) = state.identity_certificates.get_mut(&identity_name) {
            // Auto-advance PROVISIONING -> ACTIVE on the next read, matching
            // real SES once the certificate finishes provisioning (the same
            // convention `mail_from_domain_status` uses).
            for certificate in certificates.iter_mut() {
                if certificate.status == "PROVISIONING" {
                    certificate.status = "ACTIVE".to_string();
                }
            }
            certificates.sort_by(|a, b| a.from_address.cmp(&b.from_address));

            // The token is the from-address of the first item on the next
            // page (an inclusive cursor), so a disassociation between pages
            // still advances the listing instead of restarting it.
            let start_idx = match next_token {
                Some(ref token) => certificates
                    .iter()
                    .position(|c| c.from_address.as_str() >= token.as_str())
                    .unwrap_or(certificates.len()),
                None => 0,
            };

            page = certificates
                .iter()
                .skip(start_idx)
                .take(page_size)
                .map(|c| {
                    // CertificateExpiryTime is sourced from the ACM
                    // certificate on real SES. fakecloud's SES holds no
                    // handle on the ACM service, so the field is omitted
                    // rather than invented.
                    json!({
                        "FromAddress": c.from_address,
                        "Status": c.status,
                        "CertificateArn": c.certificate_arn,
                    })
                })
                .collect();
            next_marker = certificates
                .get(start_idx.saturating_add(page_size))
                .map(|c| c.from_address.clone());
        }

        let mut response = json!({ "Certificates": page });
        if let Some(next) = next_marker {
            response["NextToken"] = json!(next);
        }

        Ok(AwsResponse::json(StatusCode::OK, response.to_string()))
    }
}

/// Read a required string member out of a REST-JSON body, or build the
/// BadRequestException real SES answers with when it is missing or empty.
/// The error is boxed: `AwsResponse` is large enough that returning it inline
/// trips `clippy::result_large_err` at every call site.
fn required_body_string(body: &Value, field: &str) -> Result<String, Box<AwsResponse>> {
    match body[field].as_str() {
        Some(value) if !value.is_empty() => Ok(value.to_string()),
        Some(_) => Err(Box::new(SesV2Service::json_error(
            StatusCode::BAD_REQUEST,
            "BadRequestException",
            &format!("{field} must not be empty"),
        ))),
        None => Err(Box::new(SesV2Service::json_error(
            StatusCode::BAD_REQUEST,
            "BadRequestException",
            &format!("{field} is required"),
        ))),
    }
}

/// Validate a `CertificateArn` against the Smithy constraints: 20..=2048
/// characters shaped `arn:<partition>:<service>:<region>:<account>:certificate/<id>`.
pub(crate) fn validate_certificate_arn(arn: &str) -> Result<(), String> {
    if !(20..=2048).contains(&arn.len()) {
        return Err("CertificateArn length must be between 20 and 2048".to_string());
    }
    let parts: Vec<&str> = arn.splitn(6, ':').collect();
    let well_formed = parts.len() == 6
        && parts[0] == "arn"
        && !parts[1].is_empty()
        && !parts[2].is_empty()
        && !parts[4].is_empty()
        && parts[4].chars().all(|c| c.is_ascii_digit())
        && parts[5]
            .strip_prefix("certificate/")
            .is_some_and(|id| !id.is_empty());
    if !well_formed {
        return Err(format!(
            "CertificateArn {arn} is not a valid certificate ARN"
        ));
    }
    Ok(())
}

/// Resolve the from-address a certificate association applies to. On a
/// domain identity `FromAddress` is required and must live in that domain
/// (or a subdomain); on an email-address identity it is optional and must
/// match the identity exactly when supplied.
pub(crate) fn certificate_from_address(
    identity: &EmailIdentity,
    from_address: Option<&str>,
) -> Result<String, String> {
    if identity.identity_type == "EMAIL_ADDRESS" {
        return match from_address {
            None => Ok(identity.identity_name.clone()),
            Some(addr) if addr.eq_ignore_ascii_case(&identity.identity_name) => {
                Ok(addr.to_string())
            }
            Some(addr) => Err(format!(
                "FromAddress {addr} does not match email identity {}",
                identity.identity_name
            )),
        };
    }

    let addr = from_address.ok_or_else(|| {
        format!(
            "FromAddress is required for domain identity {}",
            identity.identity_name
        )
    })?;
    let domain = match addr.rsplit_once('@') {
        Some((local, domain)) if !local.is_empty() && !domain.is_empty() => domain,
        _ => {
            return Err(format!("FromAddress {addr} is not a valid email address"));
        }
    };
    let identity_domain = identity.identity_name.to_ascii_lowercase();
    let domain = domain.to_ascii_lowercase();
    if domain == identity_domain || domain.ends_with(&format!(".{identity_domain}")) {
        Ok(addr.to_string())
    } else {
        Err(format!(
            "FromAddress {addr} does not belong to domain identity {}",
            identity.identity_name
        ))
    }
}
