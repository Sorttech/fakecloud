//! Delivery capability checks, deliberately separate from control-plane models.
//! A modeled configuration may round-trip without its delivery being supported.

use fakecloud_core::service::AwsServiceError;
use http::StatusCode;

use crate::state::{DeliveryStream, S3Destination};

fn unsupported(setting: impl Into<String>) -> AwsServiceError {
    AwsServiceError::aws_error(
        StatusCode::BAD_REQUEST,
        "InvalidArgumentException",
        format!(
            "Firehose delivery for {} is not implemented",
            setting.into()
        ),
    )
}

pub(crate) fn validate_stream_destination(stream: &DeliveryStream) -> Result<(), AwsServiceError> {
    if let Some(setting) = stream.extra_destinations.keys().next() {
        let kind = setting.strip_suffix("Description").unwrap_or(setting);
        return Err(unsupported(format!("{kind}Configuration")));
    }
    Ok(())
}

pub(crate) fn validate_s3_delivery(
    dest: &S3Destination,
    supported_codecs: &[&str],
) -> Result<(), AwsServiceError> {
    let codec = dest.compression_format.as_deref().unwrap_or("UNCOMPRESSED");
    if !supported_codecs.contains(&codec) {
        return Err(unsupported(format!("CompressionFormat {codec}")));
    }
    if dest
        .prefix
        .as_deref()
        .is_some_and(|prefix| prefix.contains("!{"))
    {
        return Err(unsupported("Prefix expressions"));
    }
    if let Some(zone) = dest
        .custom_time_zone
        .as_deref()
        .filter(|zone| !matches!(*zone, "" | "UTC"))
    {
        return Err(unsupported(format!("CustomTimeZone {zone}")));
    }
    if let Some(processing) = dest
        .processing_configuration
        .as_ref()
        .filter(|value| value["Enabled"].as_bool() == Some(true))
    {
        let processors = processing["Processors"]
            .as_array()
            .filter(|items| !items.is_empty())
            .ok_or_else(|| {
                unsupported("ProcessingConfiguration without an AppendDelimiterToRecord processor")
            })?;
        if let Some(processor) = processors
            .iter()
            .find(|value| value["Type"] != "AppendDelimiterToRecord")
        {
            let kind = processor["Type"].as_str().unwrap_or("untyped");
            return Err(unsupported(format!(
                "ProcessingConfiguration processor {kind}"
            )));
        }
    }
    // The Firehose model defaults conversion's omitted Enabled member to true.
    if dest
        .data_format_conversion_configuration
        .as_ref()
        .is_some_and(|value| value["Enabled"].as_bool() != Some(false))
    {
        return Err(unsupported("DataFormatConversionConfiguration"));
    }
    if dest
        .dynamic_partitioning_configuration
        .as_ref()
        .is_some_and(|value| value["Enabled"].as_bool() == Some(true))
    {
        return Err(unsupported("DynamicPartitioningConfiguration"));
    }
    if let Some(mode) = dest
        .s3_backup_mode
        .as_deref()
        .filter(|mode| *mode != "Disabled")
    {
        return Err(unsupported(format!("S3BackupMode {mode}")));
    }
    if dest
        .encryption_configuration
        .as_ref()
        .is_some_and(|value| value.get("KMSEncryptionConfig").is_some())
    {
        return Err(unsupported("EncryptionConfiguration.KMSEncryptionConfig"));
    }
    Ok(())
}
