use bytes::Bytes;
use chrono::{DateTime, Utc};
use uuid::Uuid;

pub(crate) struct OutputConfig<'a> {
    pub prefix: &'a str,
    pub file_extension: Option<&'a str>,
}

/// Producers supply any record delimiters; Firehose concatenates the raw bytes.
pub(crate) fn encode_records(records: &[Vec<u8>]) -> Bytes {
    let mut payload = Vec::with_capacity(records.iter().map(Vec::len).sum());
    for record in records {
        payload.extend_from_slice(record);
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

    #[test]
    fn raw_bytes_are_verbatim() {
        assert_eq!(
            encode_records(&[b"abc".to_vec(), b"def\n".to_vec(), vec![0, 255]]).as_ref(),
            b"abcdef\n\x00\xff"
        );
        assert!(encode_records(&[vec![], vec![]]).is_empty());
    }

    #[test]
    fn static_prefix_and_suffix_use_distinct_utc_times() {
        let config = OutputConfig {
            prefix: "events/",
            file_extension: None,
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
            let config = OutputConfig { prefix, file_extension: extension };
            let now = time("2026-10-12T09:00:00Z");
            assert_eq!(object_key("events", "1", &config, now, now,
                Uuid::parse_str("12345678-1234-4234-8234-123456789abc").unwrap()), expected);
        }
    }
}
