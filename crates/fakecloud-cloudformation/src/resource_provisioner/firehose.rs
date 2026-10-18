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

/// Members of the Firehose S3 destination shapes (and every block nested in
/// them: backup configuration, buffering hints, logging options, dynamic
/// partitioning retry options, format conversion SerDes) that the Firehose
/// Smithy model types as `boolean`. No member of these names is a string.
const FIREHOSE_BOOLEAN_MEMBERS: &[&str] = &[
    "CaseInsensitive",
    "ConvertDotsInJsonKeysToUnderscores",
    "EnableDictionaryCompression",
    "EnablePadding",
    "Enabled",
];

/// The same shapes' `integer` members.
const FIREHOSE_INTEGER_MEMBERS: &[&str] = &[
    "BlockSizeBytes",
    "DurationInSeconds",
    "IntervalInSeconds",
    "MaxPaddingBytes",
    "PageSizeBytes",
    "RowIndexStride",
    "SizeInMBs",
    "StripeSizeBytes",
];

/// The same shapes' `double` members.
const FIREHOSE_DOUBLE_MEMBERS: &[&str] = &[
    "BloomFilterFalsePositiveProbability",
    "DictionaryKeyThreshold",
    "PaddingTolerance",
];

/// Map members whose keys are user data, not model member names.
const FIREHOSE_FREE_FORM_MAPS: &[&str] = &["ColumnToJsonKeyMappings"];

/// Give the modeled boolean and numeric members of a Firehose S3 destination
/// block their API types. CloudFormation passes template parameters (`Ref`)
/// as strings, so `"false"` or `"60"` would otherwise be stored and echoed
/// by `DescribeDeliveryStream` where SDKs expect a boolean or a number. A
/// string that does not parse is left as is.
pub(super) fn coerce_firehose_scalars(mut value: serde_json::Value) -> serde_json::Value {
    coerce_firehose_scalars_in_place(&mut value);
    value
}

fn coerce_firehose_scalars_in_place(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(members) => {
            for (name, member) in members.iter_mut() {
                let name = name.as_str();
                if FIREHOSE_FREE_FORM_MAPS.contains(&name) {
                    continue;
                }
                if let Some(text) = member.as_str() {
                    let text = text.trim();
                    let coerced = if FIREHOSE_BOOLEAN_MEMBERS.contains(&name) {
                        if text.eq_ignore_ascii_case("true") {
                            Some(serde_json::Value::Bool(true))
                        } else if text.eq_ignore_ascii_case("false") {
                            Some(serde_json::Value::Bool(false))
                        } else {
                            None
                        }
                    } else if FIREHOSE_INTEGER_MEMBERS.contains(&name) {
                        text.parse::<i64>().ok().map(serde_json::Value::from)
                    } else if FIREHOSE_DOUBLE_MEMBERS.contains(&name) {
                        text.parse::<f64>()
                            .ok()
                            .and_then(serde_json::Number::from_f64)
                            .map(serde_json::Value::Number)
                    } else {
                        None
                    };
                    if let Some(coerced) = coerced {
                        *member = coerced;
                    }
                } else {
                    coerce_firehose_scalars_in_place(member);
                }
            }
        }
        serde_json::Value::Array(items) => {
            items.iter_mut().for_each(coerce_firehose_scalars_in_place)
        }
        _ => {}
    }
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
    #[test]
    fn firehose_cfn_stores_processing_configuration_on_create_and_update() {
        let prov = make_provisioner();
        let with_processing = |enabled: Value| {
            json!({
                "RoleARN": "arn:aws:iam::123456789012:role/fh",
                "BucketARN": "arn:aws:s3:::bucket",
                "ProcessingConfiguration": {
                    "Enabled": enabled,
                    "Processors": [{"Type": "AppendDelimiterToRecord"}]
                }
            })
        };
        let stored = |prov: &ResourceProvisioner| {
            prov.firehose_state
                .read()
                .get("123456789012")
                .unwrap()
                .streams("us-east-1")
                .unwrap()["events"]
                .destination
                .as_ref()
                .unwrap()
                .processing_configuration
                .clone()
                .expect("ProcessingConfiguration stored")
        };

        let existing = prov
            .create_resource(&delivery_stream(with_processing(json!(true))))
            .unwrap();
        assert_eq!(stored(&prov)["Enabled"], json!(true));
        assert_eq!(
            stored(&prov)["Processors"][0]["Type"],
            "AppendDelimiterToRecord"
        );

        // Template values can arrive as strings; they are stored as booleans.
        prov.update_resource(&existing, &delivery_stream(with_processing(json!("false"))))
            .unwrap();
        assert_eq!(stored(&prov)["Enabled"], json!(false));
    }

    #[test]
    fn firehose_cfn_modeled_settings_survive_create_and_update() {
        for base in [
            "S3DestinationConfiguration",
            "ExtendedS3DestinationConfiguration",
        ] {
            let prov = make_provisioner();
            let mut existing = None;
            for (enabled, normalized, zone, backup_mode, prefix) in [
                ("TrUe", true, "America/New_York", "Enabled", "before/"),
                ("FaLsE", false, "UTC", "Disabled", "after/"),
            ] {
                let mut resource = firehose_resource(base);
                resource.properties[base] = json!({
                    "RoleARN": "arn:aws:iam::123456789012:role/fh",
                    "BucketARN": "arn:aws:s3:::bucket",
                    "Prefix": prefix,
                    "CompressionFormat": "GZIP",
                    "ProcessingConfiguration": {
                        "Enabled": enabled,
                        "Processors": [{"Type": "AppendDelimiterToRecord"}]
                    },
                    "DataFormatConversionConfiguration": {
                        "Enabled": enabled,
                        "OutputFormatConfiguration": {"Serializer": {"ParquetSerDe": {}}}
                    },
                    "DynamicPartitioningConfiguration": {
                        "Enabled": enabled, "RetryOptions": {"DurationInSeconds": 60}
                    },
                    "CloudWatchLoggingOptions": {
                        "Enabled": enabled, "LogGroupName": "delivery", "LogStreamName": prefix
                    },
                    "CustomTimeZone": zone,
                    "S3BackupMode": backup_mode,
                    "EncryptionConfiguration": {"KMSEncryptionConfig": {"AWSKMSKeyARN": "arn:aws:kms:us-east-1:123456789012:key/test"}},
                    "S3BackupConfiguration": {
                        "RoleARN": "arn:aws:iam::123456789012:role/backup",
                        "BucketARN": "arn:aws:s3:::backup",
                        "Prefix": prefix,
                        "CompressionFormat": "UNCOMPRESSED",
                        "BufferingHints": {"SizeInMBs": 5, "IntervalInSeconds": 300},
                        "CloudWatchLoggingOptions": {"Enabled": false}
                    }
                });
                if let Some(existing) = &existing {
                    prov.update_resource(existing, &resource)
                        .unwrap()
                        .expect("in-place update");
                } else {
                    existing = Some(prov.create_resource(&resource).unwrap());
                }
                let state = prov.firehose_state.read();
                let stream = &state
                    .get("123456789012")
                    .unwrap()
                    .streams("us-east-1")
                    .unwrap()["cfn-compression"];
                let dest = stream.destination.as_ref().unwrap();
                assert_eq!(
                    dest.data_format_conversion_configuration,
                    Some(json!({
                        "Enabled": normalized,
                        "OutputFormatConfiguration": {"Serializer": {"ParquetSerDe": {}}}
                    }))
                );
                assert_eq!(
                    dest.processing_configuration,
                    Some(json!({
                        "Enabled": normalized,
                        "Processors": [{"Type": "AppendDelimiterToRecord"}]
                    }))
                );
                assert_eq!(
                    dest.dynamic_partitioning_configuration,
                    Some(json!({
                        "Enabled": normalized, "RetryOptions": {"DurationInSeconds": 60}
                    }))
                );
                assert_eq!(
                    dest.cloudwatch_logging_options,
                    Some(json!({
                        "Enabled": normalized, "LogGroupName": "delivery", "LogStreamName": prefix
                    }))
                );
                assert_eq!(dest.custom_time_zone.as_deref(), Some(zone));
                assert_eq!(dest.s3_backup_mode.as_deref(), Some(backup_mode));
                assert_eq!(
                    dest.encryption_configuration,
                    Some(json!({
                        "KMSEncryptionConfig": {"AWSKMSKeyARN": "arn:aws:kms:us-east-1:123456789012:key/test"}
                    }))
                );
                assert_eq!(
                    dest.s3_backup_description,
                    Some(json!({
                        "RoleARN": "arn:aws:iam::123456789012:role/backup",
                        "BucketARN": "arn:aws:s3:::backup",
                        "Prefix": prefix,
                        "CompressionFormat": "UNCOMPRESSED",
                        "BufferingHints": {"SizeInMBs": 5, "IntervalInSeconds": 300},
                        "CloudWatchLoggingOptions": {"Enabled": false}
                    }))
                );
                assert_eq!(dest.prefix.as_deref(), Some(prefix));
                assert_eq!(dest.compression_format.as_deref(), Some("GZIP"));
                assert_eq!(stream.version_id, if normalized { "1" } else { "2" });
            }
        }
    }

    #[test]
    fn firehose_cfn_modeled_settings_defaults_and_disabled_blocks() {
        let base = destination(json!(".json"));
        let dest = parse_firehose_s3_destination(&base).unwrap();
        assert!(dest.processing_configuration.is_none());
        assert!(dest.data_format_conversion_configuration.is_none());
        assert!(dest.dynamic_partitioning_configuration.is_none());
        assert!(dest.cloudwatch_logging_options.is_none());
        assert!(dest.custom_time_zone.is_none());
        assert!(dest.s3_backup_mode.is_none());
        assert!(dest.encryption_configuration.is_none());
        assert!(dest.s3_backup_description.is_none());

        for enabled in [None, Some(json!(false)), Some(json!("false"))] {
            let mut config = base.clone();
            for field in [
                "ProcessingConfiguration",
                "DataFormatConversionConfiguration",
                "DynamicPartitioningConfiguration",
                "CloudWatchLoggingOptions",
            ] {
                config[field] = match &enabled {
                    Some(enabled) => json!({"Enabled": enabled}),
                    None => json!({}),
                };
            }
            let dest = parse_firehose_s3_destination(&config).unwrap();
            let expected = if enabled.is_some() {
                json!({"Enabled": false})
            } else {
                json!({})
            };
            assert_eq!(dest.processing_configuration, Some(expected.clone()));
            assert_eq!(
                dest.data_format_conversion_configuration,
                Some(expected.clone())
            );
            assert_eq!(
                dest.dynamic_partitioning_configuration,
                Some(expected.clone())
            );
            assert_eq!(dest.cloudwatch_logging_options, Some(expected));
        }
    }

    /// Template parameters arrive as strings; every nested modeled boolean
    /// and number is stored with its API type (so DescribeDeliveryStream
    /// echoes what SDKs deserialize), while string members, free-form map
    /// values, and unparseable strings are left untouched.
    #[test]
    fn firehose_cfn_coerces_nested_template_scalars() {
        let prov = make_provisioner();
        prov.create_resource(&delivery_stream(json!({
            "RoleARN": "arn:aws:iam::123456789012:role/fh",
            "BucketARN": "arn:aws:s3:::bucket",
            "Prefix": "60",
            "BufferingHints": {"SizeInMBs": "64", "IntervalInSeconds": "60"},
            "CloudWatchLoggingOptions": {"Enabled": "false", "LogGroupName": "true"},
            "ProcessingConfiguration": {
                "Enabled": "True",
                "Processors": [{
                    "Type": "AppendDelimiterToRecord",
                    "Parameters": [{"ParameterName": "Delimiter", "ParameterValue": "60"}]
                }]
            },
            "DataFormatConversionConfiguration": {
                "Enabled": "false",
                "InputFormatConfiguration": {"Deserializer": {"OpenXJsonSerDe": {
                    "CaseInsensitive": "true",
                    "ConvertDotsInJsonKeysToUnderscores": "FALSE",
                    "ColumnToJsonKeyMappings": {"Enabled": "true", "SizeInMBs": "1"}
                }}},
                "OutputFormatConfiguration": {"Serializer": {"OrcSerDe": {
                    "StripeSizeBytes": "8388608",
                    "BlockSizeBytes": "67108864",
                    "RowIndexStride": "1000",
                    "EnablePadding": "true",
                    "PaddingTolerance": "0.05",
                    "BloomFilterFalsePositiveProbability": "0.1",
                    "DictionaryKeyThreshold": "0.5",
                    "BloomFilterColumns": ["60"]
                }}}
            },
            "DynamicPartitioningConfiguration": {
                "Enabled": "false", "RetryOptions": {"DurationInSeconds": "300"}
            },
            "EncryptionConfiguration": {"NoEncryptionConfig": "NoEncryption"},
            "S3BackupConfiguration": {
                "RoleARN": "arn:aws:iam::123456789012:role/backup",
                "BucketARN": "arn:aws:s3:::backup",
                "BufferingHints": {"SizeInMBs": "5", "IntervalInSeconds": "not-a-number"},
                "CloudWatchLoggingOptions": {"Enabled": "true"}
            }
        })))
        .unwrap();

        let state = prov.firehose_state.read();
        let dest = state
            .get("123456789012")
            .unwrap()
            .streams("us-east-1")
            .unwrap()["events"]
            .destination
            .clone()
            .unwrap();
        assert_eq!(dest.prefix.as_deref(), Some("60"));
        assert_eq!(dest.buffering_size_mb, Some(64));
        assert_eq!(dest.buffering_interval_seconds, Some(60));
        assert_eq!(
            dest.cloudwatch_logging_options,
            Some(json!({"Enabled": false, "LogGroupName": "true"}))
        );
        let processing = dest.processing_configuration.unwrap();
        assert_eq!(processing["Enabled"], json!(true));
        assert_eq!(
            processing["Processors"][0]["Parameters"][0]["ParameterValue"],
            json!("60")
        );
        let conversion = dest.data_format_conversion_configuration.unwrap();
        assert_eq!(conversion["Enabled"], json!(false));
        let openx = &conversion["InputFormatConfiguration"]["Deserializer"]["OpenXJsonSerDe"];
        assert_eq!(openx["CaseInsensitive"], json!(true));
        assert_eq!(openx["ConvertDotsInJsonKeysToUnderscores"], json!(false));
        assert_eq!(
            openx["ColumnToJsonKeyMappings"],
            json!({"Enabled": "true", "SizeInMBs": "1"})
        );
        assert_eq!(
            conversion["OutputFormatConfiguration"]["Serializer"]["OrcSerDe"],
            json!({
                "StripeSizeBytes": 8388608,
                "BlockSizeBytes": 67108864,
                "RowIndexStride": 1000,
                "EnablePadding": true,
                "PaddingTolerance": 0.05,
                "BloomFilterFalsePositiveProbability": 0.1,
                "DictionaryKeyThreshold": 0.5,
                "BloomFilterColumns": ["60"]
            })
        );
        assert_eq!(
            dest.dynamic_partitioning_configuration,
            Some(json!({"Enabled": false, "RetryOptions": {"DurationInSeconds": 300}}))
        );
        assert_eq!(
            dest.encryption_configuration,
            Some(json!({"NoEncryptionConfig": "NoEncryption"}))
        );
        let backup = dest.s3_backup_description.unwrap();
        assert_eq!(
            backup["BufferingHints"],
            json!({"SizeInMBs": 5, "IntervalInSeconds": "not-a-number"})
        );
        assert_eq!(backup["CloudWatchLoggingOptions"], json!({"Enabled": true}));
    }

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
