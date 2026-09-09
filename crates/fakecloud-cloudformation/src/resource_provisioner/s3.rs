//! Auto-extracted from resource_provisioner/mod.rs by the
//! audit-2026-05-19 file-split. All methods here continue
//! the `impl ResourceProvisioner` block; the family slug is
//! `s3`.

use super::*;

impl ResourceProvisioner {
    pub(super) fn get_att_s3_bucket(&self, physical_id: &str, attribute: &str) -> Option<String> {
        let mut accounts = self.s3_state.write();
        let state = accounts.get_or_create(&self.account_id);
        let bucket = state.buckets.get(physical_id)?;
        match attribute {
            "Arn" => Some(Arn::s3(&bucket.name).to_string()),
            "DomainName" => Some(format!("{}.s3.amazonaws.com", bucket.name)),
            "RegionalDomainName" => {
                Some(format!("{}.s3.{}.amazonaws.com", bucket.name, self.region))
            }
            "DualStackDomainName" => Some(format!(
                "{}.s3.dualstack.{}.amazonaws.com",
                bucket.name, self.region
            )),
            "WebsiteURL" => Some(format!(
                "http://{}.s3-website-{}.amazonaws.com",
                bucket.name, self.region
            )),
            _ => None,
        }
    }

    // --- S3 ---

    pub(super) fn create_s3_bucket(
        &self,
        resource: &ResourceDefinition,
    ) -> Result<ProvisionResult, String> {
        let props = &resource.properties;
        let generated;
        let bucket_name = match props.get("BucketName").and_then(|v| v.as_str()) {
            Some(explicit) => explicit,
            None => {
                generated = generated_bucket_name(
                    self.stack_id
                        .rsplit('/')
                        .nth(1)
                        .unwrap_or(&resource.logical_id),
                    &resource.logical_id,
                    &Uuid::new_v4().to_string().replace('-', "")[..8],
                );
                &generated
            }
        };

        let mut __s3_mas = self.s3_state.write();
        let state = __s3_mas.get_or_create(&self.account_id);
        let region = self.region.clone();
        let mut bucket = S3Bucket::new(bucket_name, &self.region, &state.account_id);
        // Translate every modeled CFN property (VersioningConfiguration,
        // BucketEncryption, PublicAccessBlockConfiguration,
        // NotificationConfiguration, Tags, Website/Cors/Lifecycle/Logging) into
        // the S3 bucket state and persist each subresource through the SAME
        // store path the PutBucket* handlers use. Previously only BucketName was
        // read, so a CREATE_COMPLETE bucket surfaced with none of its
        // protections and S3->Lambda/SQS/SNS notifications never fired.
        fakecloud_s3::apply_cfn_bucket_properties(&mut bucket, props, &self.s3_store)?;
        // Write the bucket through to the S3 disk store, exactly as the real
        // CreateBucket handler does, so a CFN-provisioned bucket survives a
        // restart instead of living only in the in-memory map.
        let meta = bucket_meta_snapshot(&bucket);
        state.buckets.insert(bucket_name.to_string(), bucket);
        self.s3_store
            .put_bucket_meta(bucket_name, &meta)
            .map_err(|e| format!("failed to persist bucket {bucket_name}: {e}"))?;

        let arn = Arn::s3(bucket_name).to_string();
        let domain_name = format!("{bucket_name}.s3.amazonaws.com");
        let regional_domain_name = format!("{bucket_name}.s3.{region}.amazonaws.com");
        let dual_stack_domain_name = format!("{bucket_name}.s3.dualstack.{region}.amazonaws.com");
        let website_url = format!("http://{bucket_name}.s3-website-{region}.amazonaws.com");
        Ok(ProvisionResult::new(bucket_name)
            .with("Arn", arn)
            .with("DomainName", domain_name)
            .with("RegionalDomainName", regional_domain_name)
            .with("DualStackDomainName", dual_stack_domain_name)
            .with("WebsiteURL", website_url))
    }

    /// Apply a CFN stack update to an existing bucket in place. Re-runs the
    /// same property translation `create_s3_bucket` uses so a follow-up update
    /// that turns on versioning/encryption/etc. is applied instead of being a
    /// silent no-op. The bucket's objects are preserved; only its config
    /// (sub)resources are re-derived from the new template properties.
    pub(super) fn update_s3_bucket(
        &self,
        existing: &StackResource,
        resource: &ResourceDefinition,
    ) -> Result<ProvisionResult, String> {
        let bucket_name = &existing.physical_id;
        let mut __s3_mas = self.s3_state.write();
        let state = __s3_mas.get_or_create(&self.account_id);
        let region = self.region.clone();
        {
            let bucket = state
                .buckets
                .get_mut(bucket_name)
                .ok_or_else(|| format!("Bucket {bucket_name} not yet provisioned"))?;
            fakecloud_s3::apply_cfn_bucket_properties(
                bucket,
                &resource.properties,
                &self.s3_store,
            )?;
        }

        let arn = Arn::s3(bucket_name).to_string();
        let domain_name = format!("{bucket_name}.s3.amazonaws.com");
        let regional_domain_name = format!("{bucket_name}.s3.{region}.amazonaws.com");
        let dual_stack_domain_name = format!("{bucket_name}.s3.dualstack.{region}.amazonaws.com");
        let website_url = format!("http://{bucket_name}.s3-website-{region}.amazonaws.com");
        Ok(ProvisionResult::new(bucket_name.clone())
            .with("Arn", arn)
            .with("DomainName", domain_name)
            .with("RegionalDomainName", regional_domain_name)
            .with("DualStackDomainName", dual_stack_domain_name)
            .with("WebsiteURL", website_url))
    }

    pub(super) fn delete_s3_bucket(&self, physical_id: &str) -> Result<(), String> {
        let mut __s3_mas = self.s3_state.write();
        let state = __s3_mas.get_or_create(&self.account_id);
        state.buckets.remove(physical_id);
        // Remove the bucket from disk too, so a CFN-deleted bucket does not
        // reappear after a restart.
        self.s3_store
            .delete_bucket(physical_id)
            .map_err(|e| format!("failed to remove bucket {physical_id}: {e}"))?;
        Ok(())
    }

    // --- S3 BucketPolicy ---
    //
    // AWS::S3::BucketPolicy stores the PolicyDocument on `bucket.policy` — the
    // same field PutBucketPolicy writes — so GetBucketPolicy round-trips it.
    // The `Bucket` property is a single Ref already resolved to the bucket name
    // (which is the bucket's physical id). The policy resource's physical id is
    // `{bucket}-policy`; delete strips the suffix to recover the bucket name.

    pub(super) fn create_s3_bucket_policy(
        &self,
        resource: &ResourceDefinition,
    ) -> Result<ProvisionResult, String> {
        let bucket_name = s3_policy_bucket_name(&resource.properties)?;
        let policy = policy_document_string(&resource.properties)?;

        let mut __s3_mas = self.s3_state.write();
        let state = __s3_mas.get_or_create(&self.account_id);
        let bucket = state
            .buckets
            .get_mut(&bucket_name)
            .ok_or_else(|| format!("Bucket {bucket_name} not yet provisioned"))?;
        bucket.policy = Some(policy.clone());
        // Persist the policy subresource to disk, as PutBucketPolicy does.
        self.s3_store
            .put_bucket_subresource(&bucket_name, BucketSubresource::Policy, &policy)
            .map_err(|e| format!("failed to persist bucket policy for {bucket_name}: {e}"))?;
        Ok(ProvisionResult::new(format!("{bucket_name}-policy")))
    }

    pub(super) fn update_s3_bucket_policy(
        &self,
        existing: &StackResource,
        resource: &ResourceDefinition,
    ) -> Result<ProvisionResult, String> {
        let bucket_name = s3_policy_bucket_name(&resource.properties)?;
        let policy = policy_document_string(&resource.properties)?;

        let mut __s3_mas = self.s3_state.write();
        let state = __s3_mas.get_or_create(&self.account_id);
        // If the policy was moved to a different bucket, clear the old one.
        let old_bucket = existing
            .physical_id
            .strip_suffix("-policy")
            .unwrap_or(&existing.physical_id);
        if old_bucket != bucket_name {
            if let Some(bucket) = state.buckets.get_mut(old_bucket) {
                bucket.policy = None;
            }
            self.s3_store
                .delete_bucket_subresource(old_bucket, BucketSubresource::Policy)
                .map_err(|e| format!("failed to clear bucket policy for {old_bucket}: {e}"))?;
        }
        let bucket = state
            .buckets
            .get_mut(&bucket_name)
            .ok_or_else(|| format!("Bucket {bucket_name} not yet provisioned"))?;
        bucket.policy = Some(policy.clone());
        self.s3_store
            .put_bucket_subresource(&bucket_name, BucketSubresource::Policy, &policy)
            .map_err(|e| format!("failed to persist bucket policy for {bucket_name}: {e}"))?;
        Ok(ProvisionResult::new(format!("{bucket_name}-policy")))
    }

    pub(super) fn delete_s3_bucket_policy(&self, physical_id: &str) -> Result<(), String> {
        let bucket_name = physical_id.strip_suffix("-policy").unwrap_or(physical_id);
        let mut __s3_mas = self.s3_state.write();
        let state = __s3_mas.get_or_create(&self.account_id);
        if let Some(bucket) = state.buckets.get_mut(bucket_name) {
            bucket.policy = None;
        }
        self.s3_store
            .delete_bucket_subresource(bucket_name, BucketSubresource::Policy)
            .map_err(|e| format!("failed to remove bucket policy for {bucket_name}: {e}"))?;
        Ok(())
    }
}

/// Resolve the `Bucket` property (a Ref already resolved to the bucket name).
fn s3_policy_bucket_name(props: &serde_json::Value) -> Result<String, String> {
    props
        .get("Bucket")
        .and_then(|v| v.as_str())
        .map(String::from)
        .ok_or_else(|| "Bucket is required".to_string())
}

/// Physical name for an `AWS::S3::Bucket` that declares no `BucketName`.
///
/// Real CloudFormation generates `<stack>-<logical-id>-<random>`, and S3 bucket
/// names are lowercase-only. The logical id therefore cannot be used verbatim:
/// a CDK construct id like `SiteE53D7754` is not a legal bucket name, and while
/// path-style reads happen to tolerate it, every virtual-hosted-style read
/// fails — which is how a CloudFront distribution over an S3 origin ends up
/// serving 404 for a bucket that visibly holds the object.
fn generated_bucket_name(stack_name: &str, logical_id: &str, suffix: &str) -> String {
    fn sanitize(part: &str) -> String {
        part.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() {
                    c.to_ascii_lowercase()
                } else {
                    '-'
                }
            })
            .collect()
    }

    let mut name = format!(
        "{}-{}-{}",
        sanitize(stack_name),
        sanitize(logical_id),
        sanitize(suffix)
    );
    name.truncate(MAX_BUCKET_NAME_LEN);
    // First and last character must be alphanumeric, and the truncation above
    // can land on a separator.
    let name = name.trim_matches('-');
    if name.len() < MIN_BUCKET_NAME_LEN {
        // Nothing usable survived (an all-symbol stack and logical id); fall
        // back to something legal rather than emitting an invalid name.
        return format!("cfn-bucket-{}", sanitize(suffix));
    }
    name.to_string()
}

/// AWS general-purpose bucket name bounds.
const MIN_BUCKET_NAME_LEN: usize = 3;
const MAX_BUCKET_NAME_LEN: usize = 63;

#[cfg(test)]
mod tests {
    use super::generated_bucket_name;
    use fakecloud_s3::is_valid_bucket_name;

    #[test]
    fn generated_name_is_a_legal_bucket_name() {
        // The logical id CDK produces for a construct is mixed case; S3 bucket
        // names are lowercase-only, and an illegal name breaks every
        // virtual-hosted-style read (a CloudFront S3 origin especially).
        let name = generated_bucket_name("SpaStack", "SiteE53D7754", "1a2b3c4d");
        assert!(is_valid_bucket_name(&name), "{name}");
        assert_eq!(name, "spastack-sitee53d7754-1a2b3c4d");
    }

    #[test]
    fn generated_name_follows_the_cloudformation_shape() {
        let name = generated_bucket_name("my-stack", "Data", "beef");
        assert_eq!(name, "my-stack-data-beef");
    }

    #[test]
    fn illegal_characters_are_replaced_not_dropped() {
        let name = generated_bucket_name("My_Stack", "Bucket$Name", "0f0f");
        assert!(is_valid_bucket_name(&name), "{name}");
        assert_eq!(name, "my-stack-bucket-name-0f0f");
    }

    #[test]
    fn a_long_name_is_clamped_to_the_63_character_limit() {
        let name = generated_bucket_name(&"s".repeat(40), &"L".repeat(40), "cafe");
        assert!(is_valid_bucket_name(&name), "{name} ({} chars)", name.len());
        assert_eq!(name.len(), 63);
    }

    #[test]
    fn truncation_never_leaves_a_trailing_hyphen() {
        // Clamping can land exactly on a separator; S3 requires the last
        // character to be alphanumeric.
        let name = generated_bucket_name(&"s".repeat(62), "Bucket", "cafe");
        assert!(is_valid_bucket_name(&name), "{name}");
        assert!(
            name.ends_with(|c: char| c.is_ascii_alphanumeric()),
            "{name}"
        );
    }
}
