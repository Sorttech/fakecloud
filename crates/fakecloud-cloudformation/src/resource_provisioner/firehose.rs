//! Auto-extracted from resource_provisioner/mod.rs by the
//! audit-2026-05-19 file-split. All methods here continue
//! the `impl ResourceProvisioner` block; the family slug is
//! `firehose`.

use super::*;

/// CFN currently provisions the classic and Extended S3 configurations. Check
/// their enum inputs, including the backup blocks, with the same validator the
/// Firehose API uses before parsing or locking state; the CFN parser otherwise
/// silently discards non-string enum values.
fn validate_firehose_compression(props: &serde_json::Value) -> Result<(), String> {
    for member in [
        "S3DestinationConfiguration",
        "ExtendedS3DestinationConfiguration",
    ] {
        fakecloud_firehose::validate_s3_destination_compression(&props[member], false)
            .map_err(|err| err.to_string())?;
    }
    Ok(())
}

impl ResourceProvisioner {
    // --- Firehose ---

    pub(super) fn create_firehose_delivery_stream(
        &self,
        resource: &ResourceDefinition,
    ) -> Result<ProvisionResult, String> {
        let props = &resource.properties;
        validate_firehose_compression(props)?;
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
        validate_firehose_compression(props)?;
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

    fn firehose_resource(base: &str) -> ResourceDefinition {
        let mut props = json!({
            "DeliveryStreamName": "cfn-compression",
            "Tags": [{"Key": "env", "Value": "before"}]
        });
        props[base] = json!({
            "RoleARN": "arn:aws:iam::123456789012:role/fh",
            "BucketARN": "arn:aws:s3:::bucket", "Prefix": "before/"
        });
        make_resource("AWS::KinesisFirehose::DeliveryStream", "Stream", props)
    }

    fn firehose_state(prov: &ResourceProvisioner) -> Value {
        serde_json::to_value(&*prov.firehose_state.read()).unwrap()
    }

    #[test]
    fn firehose_compression_invalid_create_is_resource_failure() {
        for base in [
            "S3DestinationConfiguration",
            "ExtendedS3DestinationConfiguration",
        ] {
            for bad in [
                json!("BROTLI"),
                json!("SNAPPY"),
                json!(null),
                json!(123),
                json!({}),
            ] {
                let prov = make_provisioner();
                let before = firehose_state(&prov);
                let mut resource = firehose_resource(base);
                resource.properties[base]["CompressionFormat"] = bad;
                let err = prov
                    .create_resource(&resource)
                    .expect_err("invalid enum must fail provisioning");
                assert!(err.contains("CompressionFormat"), "{err}");
                assert_eq!(firehose_state(&prov), before);
            }
        }
    }

    #[test]
    fn firehose_compression_invalid_update_preserves_original_stream() {
        for base in [
            "S3DestinationConfiguration",
            "ExtendedS3DestinationConfiguration",
        ] {
            let prov = make_provisioner();
            let resource = firehose_resource(base);
            let existing = prov.create_resource(&resource).unwrap();
            let before = firehose_state(&prov);
            for bad in [json!("BROTLI"), json!("gzip"), json!(false)] {
                let mut changed = firehose_resource(base);
                changed.properties[base]["Prefix"] = json!("changed/");
                changed.properties[base]["CompressionFormat"] = bad;
                changed.properties["Tags"] = json!([{ "Key": "env", "Value": "changed" }]);
                assert!(prov.update_resource(&existing, &changed).is_err());
                assert_eq!(firehose_state(&prov), before);
            }
        }
    }

    #[test]
    fn firehose_compression_invalid_backup_update_preserves_original_stream() {
        let prov = make_provisioner();
        let base = "ExtendedS3DestinationConfiguration";
        let resource = firehose_resource(base);
        let existing = prov.create_resource(&resource).unwrap();
        let before = firehose_state(&prov);
        let mut changed = firehose_resource(base);
        changed.properties[base]["Prefix"] = json!("changed/");
        changed.properties[base]["S3BackupConfiguration"] = json!({"CompressionFormat": 42});
        assert!(prov.update_resource(&existing, &changed).is_err());
        assert_eq!(firehose_state(&prov), before);
    }

    #[test]
    fn firehose_compression_backup_validated_for_both_s3_shapes() {
        for base in [
            "S3DestinationConfiguration",
            "ExtendedS3DestinationConfiguration",
        ] {
            let prov = make_provisioner();
            let mut resource = firehose_resource(base);
            resource.properties[base]["S3BackupConfiguration"] =
                json!({"CompressionFormat": "BROTLI"});
            let err = prov
                .create_resource(&resource)
                .expect_err("invalid backup enum must fail provisioning");
            assert!(err.contains("CompressionFormat"), "{base}: {err}");
            assert!(prov.firehose_state.read().accounts.is_empty());
        }
    }

    #[test]
    fn firehose_compression_invalid_backup_create_is_resource_failure() {
        let prov = make_provisioner();
        let mut resource = firehose_resource("ExtendedS3DestinationConfiguration");
        resource.properties["ExtendedS3DestinationConfiguration"]["S3BackupConfiguration"] =
            json!({"CompressionFormat": "BROTLI"});
        assert!(prov.create_resource(&resource).is_err());
        assert!(prov.firehose_state.read().accounts.is_empty());
    }

    #[test]
    fn firehose_compression_valid_values_and_defaults_provision() {
        for base in [
            "S3DestinationConfiguration",
            "ExtendedS3DestinationConfiguration",
        ] {
            for format in [
                None,
                Some("UNCOMPRESSED"),
                Some("GZIP"),
                Some("ZIP"),
                Some("Snappy"),
                Some("HADOOP_SNAPPY"),
            ] {
                let prov = make_provisioner();
                let mut resource = firehose_resource(base);
                if let Some(format) = format {
                    resource.properties[base]["CompressionFormat"] = json!(format);
                }
                let existing = prov.create_resource(&resource).unwrap();
                prov.update_resource(&existing, &resource)
                    .unwrap()
                    .expect("in-place update");
                let state = prov.firehose_state.read();
                let stream = &state
                    .get("123456789012")
                    .unwrap()
                    .streams("us-east-1")
                    .unwrap()["cfn-compression"];
                assert_eq!(
                    stream
                        .destination
                        .as_ref()
                        .unwrap()
                        .compression_format
                        .as_deref(),
                    format
                );
                assert_eq!(stream.version_id, "2");
            }
        }
    }
}
