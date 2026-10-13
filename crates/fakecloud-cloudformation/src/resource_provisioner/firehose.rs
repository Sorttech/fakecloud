//! Auto-extracted from resource_provisioner/mod.rs by the
//! audit-2026-05-19 file-split. All methods here continue
//! the `impl ResourceProvisioner` block; the family slug is
//! `firehose`.

use super::*;

impl ResourceProvisioner {
    // --- Firehose ---

    pub(super) fn create_firehose_delivery_stream(
        &self,
        resource: &ResourceDefinition,
    ) -> Result<ProvisionResult, String> {
        let props = &resource.properties;
        let generated_name = self.physical_name(resource);
        let name = props
            .get("DeliveryStreamName")
            .and_then(|v| v.as_str())
            .unwrap_or(&generated_name)
            .to_string();

        let arn = self.regional_arn("firehose", &format!("deliverystream/{}", name));
        let stream_type = props
            .get("DeliveryStreamType")
            .and_then(|v| v.as_str())
            .unwrap_or("DirectPut")
            .to_string();

        let has_s3 = props.get("S3DestinationConfiguration").is_some();
        let has_extended_s3 = props.get("ExtendedS3DestinationConfiguration").is_some();
        if has_s3 && has_extended_s3 {
            return Err("Only one of S3DestinationConfiguration or ExtendedS3DestinationConfiguration may be set".to_string());
        }
        let destination = Some(if let Some(s3) = props.get("S3DestinationConfiguration") {
            parse_firehose_s3_destination(s3)?
        } else if let Some(s3) = props.get("ExtendedS3DestinationConfiguration") {
            parse_firehose_s3_destination(s3)?
        } else {
            return Err("Delivery stream requires a destination configuration".to_string());
        });

        let mut tags = BTreeMap::new();
        if let Some(arr) = props.get("Tags").and_then(|v| v.as_array()) {
            for tag in arr {
                if let (Some(k), Some(v)) = (
                    tag.get("Key").and_then(|v| v.as_str()),
                    tag.get("Value").and_then(|v| v.as_str()),
                ) {
                    tags.insert(k.to_string(), v.to_string());
                }
            }
        }

        let stream = DeliveryStream {
            name: name.clone(),
            arn: arn.clone(),
            status: "ACTIVE".to_string(),
            stream_type: stream_type.clone(),
            created_at: Utc::now(),
            last_update: Utc::now(),
            version_id: "1".to_string(),
            destination,
            tags,
            encryption: None,
            extra_destinations: std::collections::BTreeMap::new(),
        };

        let mut state = self.firehose_state.write();
        let account = state.get_or_create(&self.account_id, &self.region);
        account
            .streams_mut(&self.region)
            .insert(name.clone(), stream);

        let mut attributes = BTreeMap::new();
        attributes.insert("Arn".to_string(), arn.clone());
        attributes.insert("DeliveryStreamName".to_string(), name.clone());

        Ok(ProvisionResult {
            physical_id: name,
            attributes,
        })
    }

    /// In-place `UpdateStack` for an `AWS::KinesisFirehose::DeliveryStream`.
    /// Mutates the stored `DeliveryStream` in place instead of the reprovision
    /// fallback's delete+recreate. Applies the mutable destination config + tags
    /// (matching `UpdateDestination`/tag ops), bumps the `version_id`, and
    /// preserves the identity (`arn`, `created_at`, `stream_type`) and any
    /// `extra_destinations` on the record.
    pub(super) fn update_firehose_delivery_stream(
        &self,
        existing: &StackResource,
        resource: &ResourceDefinition,
    ) -> Result<ProvisionResult, String> {
        let props = &resource.properties;
        let name = existing.physical_id.clone();

        let has_s3 = props.get("S3DestinationConfiguration").is_some();
        let has_extended_s3 = props.get("ExtendedS3DestinationConfiguration").is_some();
        if has_s3 && has_extended_s3 {
            return Err("Only one of S3DestinationConfiguration or ExtendedS3DestinationConfiguration may be set".to_string());
        }
        let new_destination = if let Some(s3) = props.get("S3DestinationConfiguration") {
            Some(parse_firehose_s3_destination(s3)?)
        } else if let Some(s3) = props.get("ExtendedS3DestinationConfiguration") {
            Some(parse_firehose_s3_destination(s3)?)
        } else {
            None
        };

        let mut state = self.firehose_state.write();
        let account = state.get_or_create(&self.account_id, &self.region);
        let stream = account
            .streams_mut(&self.region)
            .get_mut(&name)
            .ok_or_else(|| format!("Delivery stream {name} not yet provisioned"))?;

        if let Some(dest) = new_destination {
            stream.destination = Some(dest);
        }
        if props.get("Tags").is_some() {
            let mut tags = BTreeMap::new();
            if let Some(arr) = props.get("Tags").and_then(|v| v.as_array()) {
                for tag in arr {
                    if let (Some(k), Some(v)) = (
                        tag.get("Key").and_then(|v| v.as_str()),
                        tag.get("Value").and_then(|v| v.as_str()),
                    ) {
                        tags.insert(k.to_string(), v.to_string());
                    }
                }
            }
            stream.tags = tags;
        }
        stream.last_update = Utc::now();
        let next_version = stream.version_id.parse::<u64>().unwrap_or(1) + 1;
        stream.version_id = next_version.to_string();
        let arn = stream.arn.clone();

        let mut attributes = BTreeMap::new();
        attributes.insert("Arn".to_string(), arn);
        attributes.insert("DeliveryStreamName".to_string(), name.clone());
        Ok(ProvisionResult {
            physical_id: name,
            attributes,
        })
    }

    pub(super) fn delete_firehose_delivery_stream(&self, physical_id: &str) -> Result<(), String> {
        let mut state = self.firehose_state.write();
        let account = state.get_or_create(&self.account_id, &self.region);
        account.streams_mut(&self.region).remove(physical_id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{make_provisioner, make_resource};
    use super::*;
    use serde_json::{json, Value};

    fn delivery_stream(extended_s3: Value) -> ResourceDefinition {
        make_resource(
            "AWS::KinesisFirehose::DeliveryStream",
            "Firehose",
            json!({
                "DeliveryStreamName": "events",
                "ExtendedS3DestinationConfiguration": extended_s3,
            }),
        )
    }

    fn destination(file_extension: Value) -> Value {
        json!({
            "RoleARN": "arn:aws:iam::123456789012:role/fh",
            "BucketARN": "arn:aws:s3:::bucket",
            "FileExtension": file_extension,
        })
    }

    fn stored_file_extension(prov: &ResourceProvisioner) -> Option<String> {
        prov.firehose_state
            .read()
            .get("123456789012")
            .unwrap()
            .streams("us-east-1")
            .unwrap()["events"]
            .destination
            .as_ref()
            .unwrap()
            .file_extension
            .clone()
    }

    #[test]
    fn firehose_cfn_stores_file_extension_on_create_and_update() {
        let prov = make_provisioner();
        let existing = prov
            .create_resource(&delivery_stream(destination(json!(".json"))))
            .unwrap();
        assert_eq!(stored_file_extension(&prov).as_deref(), Some(".json"));

        prov.update_resource(&existing, &delivery_stream(destination(json!(".log"))))
            .unwrap();
        assert_eq!(stored_file_extension(&prov).as_deref(), Some(".log"));
    }

    #[test]
    fn firehose_cfn_rejects_invalid_file_extension() {
        let prov = make_provisioner();
        for invalid in [json!("json"), json!(".JSON"), json!(1)] {
            let err = prov
                .create_resource(&delivery_stream(destination(invalid.clone())))
                .expect_err("invalid FileExtension must fail the resource");
            assert!(err.contains("FileExtension"), "{err}");
        }
        let existing = prov
            .create_resource(&delivery_stream(destination(json!(".json"))))
            .unwrap();
        let err = prov
            .update_resource(&existing, &delivery_stream(destination(json!(".JSON"))))
            .expect_err("invalid FileExtension must fail the update");
        assert!(err.contains("FileExtension"), "{err}");
        assert_eq!(stored_file_extension(&prov).as_deref(), Some(".json"));
    }
}
