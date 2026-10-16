//! CloudFormation dynamic references: `{{resolve:ssm:...}}`,
//! `{{resolve:ssm-secure:...}}` and `{{resolve:secretsmanager:...}}` in
//! resource properties, resolved from this account's SSM Parameter Store and
//! Secrets Manager state as each resource is created or updated.
//!
//! AWS behaviour, from the CloudFormation User Guide (fetched 2026-10-16):
//!
//! - <https://docs.aws.amazon.com/AWSCloudFormation/latest/UserGuide/dynamic-references.html>:
//!   "CloudFormation retrieves the value of the specified reference when
//!   necessary during stack and change set operations."
//! - <https://docs.aws.amazon.com/AWSCloudFormation/latest/UserGuide/dynamic-references-ssm.html>:
//!   "Your reference must adhere to the following regular expression pattern
//!   for parameter-name and version: `{{resolve:ssm:[a-zA-Z0-9_.\-/]+(:\d+)?}}`"
//!   and "If you don't specify the exact version, CloudFormation uses the
//!   latest version of the parameter whenever you create or update the stack."
//!   The ssm-secure page gives the same pattern with `ssm-secure`.
//! - <https://docs.aws.amazon.com/AWSCloudFormation/latest/UserGuide/dynamic-references-secretsmanager.html>:
//!   "`{{resolve:secretsmanager:secret-id:secret-string:json-key:version-stage:version-id}}`",
//!   secret-id is "The name or ARN of the secret", secret-string "The only
//!   supported value is SecretString", and "If you don't specify a json-key,
//!   CloudFormation retrieves the entire secret text." Both
//!   `{{resolve:secretsmanager:MySecret}}` and
//!   `{{resolve:secretsmanager:MySecret::::}}` are given as examples.
//! - The same page: "CloudFormation retrieves the secret value only during
//!   resource creation or updates that modify the resource containing the
//!   dynamic reference." Resolution therefore happens here, after the update
//!   path has compared the unresolved properties, so a changed secret alone
//!   never updates a resource.

use serde_json::Value;

use super::*;

const PREFIX: &str = "{{resolve:";

/// The parts of a `secretsmanager` reference after `{{resolve:secretsmanager:`.
struct SecretReference<'a> {
    secret_id: &'a str,
    json_key: Option<&'a str>,
    version_stage: Option<&'a str>,
    version_id: Option<&'a str>,
}

impl ResourceProvisioner {
    /// Replace every dynamic reference in `resource`'s properties with the
    /// value it names. A reference may be a whole property value or embedded
    /// in a longer string, any number of times.
    pub(crate) fn resolve_dynamic_references(
        &self,
        mut resource: ResourceDefinition,
    ) -> Result<ResourceDefinition, String> {
        self.resolve_dynamic_value(&mut resource.properties)?;
        Ok(resource)
    }

    fn resolve_dynamic_value(&self, value: &mut Value) -> Result<(), String> {
        match value {
            Value::String(s) if s.contains(PREFIX) => *s = self.resolve_dynamic_string(s)?,
            Value::Array(items) => {
                for item in items {
                    self.resolve_dynamic_value(item)?;
                }
            }
            Value::Object(map) => {
                for item in map.values_mut() {
                    self.resolve_dynamic_value(item)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn resolve_dynamic_string(&self, s: &str) -> Result<String, String> {
        let mut out = String::with_capacity(s.len());
        let mut rest = s;
        while let Some(start) = rest.find(PREFIX) {
            let body = &rest[start + PREFIX.len()..];
            let Some(len) = body.find("}}") else {
                break;
            };
            out.push_str(&rest[..start]);
            let end = start + PREFIX.len() + len + 2;
            match self.resolve_dynamic_reference(&body[..len])? {
                Some(resolved) => out.push_str(&resolved),
                // Not a recognised reference form: kept as the literal text.
                None => out.push_str(&rest[start..end]),
            }
            rest = &rest[end..];
        }
        out.push_str(rest);
        Ok(out)
    }

    /// Resolve one reference body (`ssm:name:1`, `secretsmanager:id:...`);
    /// `None` when it is not one of the documented forms.
    fn resolve_dynamic_reference(&self, reference: &str) -> Result<Option<String>, String> {
        let Some((service, key)) = reference.split_once(':') else {
            return Ok(None);
        };
        match service {
            "ssm" | "ssm-secure" if is_ssm_reference_key(key) => self.ssm_reference(key).map(Some),
            "secretsmanager" => match parse_secret_reference(key) {
                Some(secret) => self.secret_reference(&secret).map(Some),
                None => Ok(None),
            },
            _ => Ok(None),
        }
    }

    fn ssm_reference(&self, key: &str) -> Result<String, String> {
        fakecloud_ssm::read_parameter_value(
            &self.ssm_state,
            self.kms_hook.as_deref(),
            &self.account_id,
            &self.region,
            key,
        )
        .map(|p| p.value)
        .map_err(|e| {
            if e.code() == "ParameterNotFound" {
                // Not in the User Guide; the text CloudFormation reports for
                // a missing parameter, quoted from
                // https://github.com/aws/aws-cdk/discussions/22281 (fetched
                // 2026-10-16): "Unable to fetch parameters [/test/param] from
                // parameter store for this account."
                let name = key.split(':').next().unwrap_or(key);
                format!(
                    "Unable to fetch parameters [{name}] from parameter store for this account."
                )
            } else {
                format!("{}: {}", e.code(), e.message())
            }
        })
    }

    fn secret_reference(&self, secret: &SecretReference<'_>) -> Result<String, String> {
        let value = fakecloud_secretsmanager::value::read_secret_value(
            &self.secretsmanager_state,
            self.kms_hook.as_deref(),
            &self.account_id,
            &self.region,
            secret.secret_id,
            secret.version_id,
            secret.version_stage,
        )
        .map_err(|e| {
            // CloudFormation reports the Secrets Manager error in the AWS SDK
            // for Java form, quoted from
            // https://github.com/aws/aws-cdk/issues/24130 (fetched
            // 2026-10-16): "Secrets Manager can't find the specified secret.
            // (Service: AWSSecretsManager; Status Code: 400; Error Code:
            // ResourceNotFoundException; Request ID: ...; Proxy: null)"
            format!(
                "{} (Service: AWSSecretsManager; Status Code: {}; Error Code: {}; Request ID: {}; Proxy: null)",
                e.message(),
                e.status().as_u16(),
                e.code(),
                Uuid::new_v4()
            )
        })?;
        // The User Guide documents no error text for a binary secret or a
        // missing json-key; these name the problem plainly.
        let text = value
            .secret_string
            .ok_or_else(|| format!("Secret {} has no SecretString", secret.secret_id))?;
        let Some(key) = secret.json_key else {
            return Ok(text);
        };
        let fields: serde_json::Map<String, Value> = serde_json::from_str(&text).map_err(|_| {
            format!(
                "Secret {} SecretString is not a JSON object",
                secret.secret_id
            )
        })?;
        match fields.get(key) {
            Some(Value::String(s)) => Ok(s.clone()),
            Some(other) => Ok(other.to_string()),
            None => Err(format!(
                "Secret {} SecretString has no key {key}",
                secret.secret_id
            )),
        }
    }
}

/// `parameter-name(:version)?` per the documented pattern
/// `[a-zA-Z0-9_.\-/]+(:\d+)?`.
fn is_ssm_reference_key(key: &str) -> bool {
    let (name, version) = match key.split_once(':') {
        Some((name, version)) => (name, Some(version)),
        None => (key, None),
    };
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-' | '/'))
        && version.is_none_or(|v| !v.is_empty() && v.chars().all(|c| c.is_ascii_digit()))
}

/// Split `secret-id[:SecretString[:json-key[:version-stage[:version-id]]]]`,
/// where an empty segment means "not specified" and a secret-id given as an
/// ARN (`arn:partition:secretsmanager:region:account:secret:name`) carries
/// six colons of its own.
fn parse_secret_reference(key: &str) -> Option<SecretReference<'_>> {
    let id_len = if key.starts_with("arn:") {
        key.match_indices(':').nth(6).map_or(key.len(), |(i, _)| i)
    } else {
        key.find(':').unwrap_or(key.len())
    };
    let (secret_id, rest) = key.split_at(id_len);
    let mut segments = rest
        .strip_prefix(':')
        .into_iter()
        .flat_map(|r| r.split(':'))
        .map(|s| (!s.is_empty()).then_some(s));
    let secret_string = segments.next().flatten();
    let json_key = segments.next().flatten();
    let version_stage = segments.next().flatten();
    let version_id = segments.next().flatten();
    let valid = !secret_id.is_empty()
        && secret_string.is_none_or(|s| s == "SecretString")
        && segments.next().is_none();
    valid.then_some(SecretReference {
        secret_id,
        json_key,
        version_stage,
        version_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssm_reference_key_follows_the_documented_pattern() {
        assert!(is_ssm_reference_key(
            "/aws/service/ami-amazon-linux-latest/al2023"
        ));
        assert!(is_ssm_reference_key("golden-ami:2"));
        assert!(!is_ssm_reference_key("golden-ami:label"));
        assert!(!is_ssm_reference_key("golden-ami:"));
        assert!(!is_ssm_reference_key(""));
    }

    #[test]
    fn secret_reference_splits_name_and_arn_ids() {
        let r = parse_secret_reference("MySecret").unwrap();
        assert_eq!(r.secret_id, "MySecret");
        assert!(r.json_key.is_none() && r.version_stage.is_none() && r.version_id.is_none());

        let r = parse_secret_reference("MySecret::::").unwrap();
        assert_eq!(r.secret_id, "MySecret");
        assert!(r.json_key.is_none());

        let r = parse_secret_reference("MySecret:SecretString:password:AWSPREVIOUS").unwrap();
        assert_eq!(r.json_key, Some("password"));
        assert_eq!(r.version_stage, Some("AWSPREVIOUS"));

        let arn = "arn:aws:secretsmanager:us-west-2:123456789012:secret:MySecret-a1b2c3";
        let cdk = format!("{arn}:SecretString:password::");
        let r = parse_secret_reference(&cdk).unwrap();
        assert_eq!(r.secret_id, arn);
        assert_eq!(r.json_key, Some("password"));
        assert!(r.version_stage.is_none() && r.version_id.is_none());

        assert_eq!(parse_secret_reference(arn).unwrap().secret_id, arn);
        assert!(parse_secret_reference("MySecret:SecretBinary").is_none());
        assert!(parse_secret_reference("MySecret:SecretString:k:s:i:extra").is_none());
    }
}
