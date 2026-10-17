use bytes::Bytes;
use chrono::{DateTime, Utc};
use serde_json::Value;
use uuid::Uuid;

pub(crate) struct OutputConfig<'a> {
    pub prefix: &'a str,
    pub file_extension: Option<&'a str>,
    /// The built-in `AppendDelimiterToRecord` processor: write `\n` after
    /// every record.
    pub append_delimiter: bool,
}

/// Whether an S3 destination's `ProcessingConfiguration` enables the built-in
/// `AppendDelimiterToRecord` processor. Only `Enabled: true` counts. AWS
/// documents only a new line delimiter, so a `Delimiter` parameter is not
/// interpreted. Other processor types are not run.
pub(crate) fn append_delimiter_enabled(processing_configuration: Option<&Value>) -> bool {
    processing_configuration.is_some_and(|processing| {
        processing["Enabled"].as_bool() == Some(true)
            && processing["Processors"]
                .as_array()
                .is_some_and(|processors| {
                    processors
                        .iter()
                        .any(|processor| processor["Type"] == "AppendDelimiterToRecord")
                })
    })
}

/// Firehose concatenates the raw record bytes; producers supply any record
/// delimiters unless `AppendDelimiterToRecord` adds `\n` after each record.
pub(crate) fn encode_records(records: &[Vec<u8>], config: &OutputConfig<'_>) -> Bytes {
    let delimiter: &[u8] = if config.append_delimiter { b"\n" } else { b"" };
    let mut payload = Vec::with_capacity(
        records
            .iter()
            .map(|record| record.len() + delimiter.len())
            .sum(),
    );
    for record in records {
        payload.extend_from_slice(record);
        payload.extend_from_slice(delimiter);
    }
    Bytes::from(payload)
}

/// S3 keys are logical names, not filesystem paths. Do not normalize the prefix
/// or insert a separator between it and the default UTC time hierarchy.
pub(crate) fn object_key(
    stream: &str,
    version: &str,
    config: &OutputConfig<'_>,
    oldest_arrival: DateTime<Utc>,
    delivery_time: DateTime<Utc>,
    uuid: Uuid,
) -> String {
    format!(
        "{}{}/{stream}-{version}-{}-{uuid}{}",
        config.prefix,
        oldest_arrival.format("%Y/%m/%d/%H"),
        delivery_time.format("%Y-%m-%d-%H-%M-%S"),
        config.file_extension.unwrap_or_default(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};
    use uuid::Uuid;

    fn time(value: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(value)
            .unwrap()
            .with_timezone(&Utc)
    }

    fn config(append_delimiter: bool) -> OutputConfig<'static> {
        OutputConfig {
            prefix: "",
            file_extension: None,
            append_delimiter,
        }
    }

    #[test]
    fn raw_bytes_are_verbatim() {
        assert_eq!(
            encode_records(
                &[b"abc".to_vec(), b"def\n".to_vec(), vec![0, 255]],
                &config(false)
            )
            .as_ref(),
            b"abcdef\n\x00\xff"
        );
        assert!(encode_records(&[vec![], vec![]], &config(false)).is_empty());
    }

    #[test]
    fn append_delimiter_terminates_every_record() {
        assert_eq!(
            encode_records(
                &[b"a".to_vec(), b"b\n".to_vec(), vec![0, 255]],
                &config(true)
            )
            .as_ref(),
            b"a\nb\n\n\x00\xff\n"
        );
        assert_eq!(encode_records(&[vec![]], &config(true)).as_ref(), b"\n");
        assert!(encode_records(&[], &config(true)).is_empty());
    }

    #[test]
    fn append_delimiter_requires_enabled_processor() {
        use serde_json::json;

        for (processing, expected) in [
            (
                json!({"Enabled": true, "Processors": [{"Type": "AppendDelimiterToRecord"}]}),
                true,
            ),
            (
                json!({"Enabled": true, "Processors": [{"Type": "Lambda"}, {"Type": "AppendDelimiterToRecord", "Parameters": [{"ParameterName": "Delimiter", "ParameterValue": "\\n"}]}]}),
                true,
            ),
            (
                json!({"Enabled": false, "Processors": [{"Type": "AppendDelimiterToRecord"}]}),
                false,
            ),
            (
                json!({"Processors": [{"Type": "AppendDelimiterToRecord"}]}),
                false,
            ),
            (
                json!({"Enabled": "true", "Processors": [{"Type": "AppendDelimiterToRecord"}]}),
                false,
            ),
            (
                json!({"Enabled": true, "Processors": [{"Type": "Lambda"}]}),
                false,
            ),
            (
                json!({"Enabled": true, "Processors": {"Type": "AppendDelimiterToRecord"}}),
                false,
            ),
            (json!({"Enabled": true}), false),
        ] {
            assert_eq!(
                append_delimiter_enabled(Some(&processing)),
                expected,
                "{processing}"
            );
        }
        assert!(!append_delimiter_enabled(None));
    }

    #[test]
    fn static_prefix_and_suffix_use_distinct_utc_times() {
        let config = OutputConfig {
            prefix: "events/",
            file_extension: None,
            append_delimiter: false,
        };
        let key = object_key(
            "events",
            "2",
            &config,
            time("2026-10-12T09:59:59Z"),
            time("2026-10-12T10:00:02Z"),
            Uuid::parse_str("12345678-1234-4234-8234-123456789abc").unwrap(),
        );
        assert_eq!(key,
            "events/2026/10/12/09/events-2-2026-10-12-10-00-02-12345678-1234-4234-8234-123456789abc");
    }

    #[test]
    fn prefixes_and_extensions_are_appended_verbatim() {
        for (prefix, extension, expected) in [
            ("", None, "2026/10/12/09/events-1-2026-10-12-09-00-00-12345678-1234-4234-8234-123456789abc"),
            ("events", Some(""), "events2026/10/12/09/events-1-2026-10-12-09-00-00-12345678-1234-4234-8234-123456789abc"),
            ("events/", Some(".custom.log"), "events/2026/10/12/09/events-1-2026-10-12-09-00-00-12345678-1234-4234-8234-123456789abc.custom.log"),
        ] {
            let config = OutputConfig { prefix, file_extension: extension, append_delimiter: false };
            let now = time("2026-10-12T09:00:00Z");
            assert_eq!(object_key("events", "1", &config, now, now,
                Uuid::parse_str("12345678-1234-4234-8234-123456789abc").unwrap()), expected);
        }
    }
}
