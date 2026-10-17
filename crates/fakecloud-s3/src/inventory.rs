use bytes::Bytes;
use chrono::Utc;
use md5::{Digest, Md5};

use fakecloud_persistence::S3Store;

use crate::state::{S3Object, SharedS3State};
use crate::xml_util::extract_tag;

/// Parsed inventory destination from the inventory configuration XML.
struct InventoryDestination {
    bucket_arn: String,
    prefix: Option<String>,
}

/// Parse the destination from an `<InventoryConfiguration>` XML body.
fn parse_inventory_destination(xml: &str) -> Option<InventoryDestination> {
    // Locate each closing tag relative to its opening tag so a malformed body
    // (closing before opening, or a duplicated/inverted tag) can't slice with
    // begin > end and panic — a reachable DoS under eager delivery.
    let dest_content = xml.find("<Destination>")? + "<Destination>".len();
    let dest_end = xml[dest_content..].find("</Destination>")? + dest_content;
    let dest_body = &xml[dest_content..dest_end];

    // Look for <S3BucketDestination>
    let s3_content = dest_body.find("<S3BucketDestination>")? + "<S3BucketDestination>".len();
    let s3_end = dest_body[s3_content..].find("</S3BucketDestination>")? + s3_content;
    let s3_body = &dest_body[s3_content..s3_end];

    let bucket_arn = extract_tag(s3_body, "Bucket")?;
    let prefix = extract_tag(s3_body, "Prefix");

    Some(InventoryDestination { bucket_arn, prefix })
}

/// Extract the bucket name from an ARN like `arn:aws:s3:::my-bucket`.
fn bucket_name_from_arn(arn: &str) -> Option<&str> {
    fakecloud_aws::arn::arn_resource(arn, "s3").and_then(|rest| rest.strip_prefix("::"))
}

/// Generate an inventory report for a bucket and store it in the destination.
///
/// The report is a CSV with columns: Bucket, Key, Size, LastModifiedDate, ETag, StorageClass.
/// It is written as a new object version in the destination bucket, the way
/// PutObject writes one, and persisted through `store` when one is configured.
pub fn generate_inventory_report(
    state: &SharedS3State,
    store: Option<&dyn S3Store>,
    source_bucket: &str,
    config_id: &str,
) {
    // Read the inventory config (search all accounts)
    let (config_xml, source_account_id) = {
        let mas = state.read();
        let mut found = None;
        for (acct_id, st) in mas.iter() {
            if let Some(cfg) = st
                .buckets
                .get(source_bucket)
                .and_then(|b| b.inventory_configs.get(config_id).cloned())
            {
                found = Some((cfg, acct_id.to_string()));
                break;
            }
        }
        match found {
            Some((cfg, acct)) => (Some(cfg), acct),
            None => (None, String::new()),
        }
    };

    let config_xml = match config_xml {
        Some(c) => c,
        None => return,
    };

    let destination = match parse_inventory_destination(&config_xml) {
        Some(d) => d,
        None => return,
    };

    let dest_bucket_name = match bucket_name_from_arn(&destination.bucket_arn) {
        Some(name) => name.to_string(),
        None => return,
    };

    // Collect object data from source bucket
    let rows: Vec<String> = {
        let mas = state.read();
        let st = match mas.get(&source_account_id) {
            Some(s) => s,
            None => return,
        };
        let bucket = match st.buckets.get(source_bucket) {
            Some(b) => b,
            None => return,
        };

        let mut csv_rows = vec![
            "\"Bucket\",\"Key\",\"Size\",\"LastModifiedDate\",\"ETag\",\"StorageClass\""
                .to_string(),
        ];

        for (key, obj) in &bucket.objects {
            if obj.is_delete_marker {
                continue;
            }
            csv_rows.push(format!(
                "{},{},{},{},{},{}",
                csv_escape(source_bucket),
                csv_escape(key),
                obj.size,
                csv_escape(
                    &obj.last_modified
                        .format("%Y-%m-%dT%H:%M:%S%.3fZ")
                        .to_string()
                ),
                csv_escape(&obj.etag),
                csv_escape(&obj.storage_class),
            ));
        }

        csv_rows
    };

    let csv_content = rows.join("\n") + "\n";
    let data = Bytes::from(csv_content);
    let size = data.len() as u64;
    let etag = format!("{:x}", Md5::digest(&data));
    let now = Utc::now();

    let report_key = format!(
        "{}{}/{}/data/{}.csv",
        destination.prefix.as_deref().unwrap_or(""),
        source_bucket,
        config_id,
        now.format("%Y-%m-%dT%H-%M-%SZ"),
    );

    let report_object = S3Object {
        key: report_key.clone(),
        content_type: "text/csv".to_string(),
        etag,
        size,
        last_modified: now,
        storage_class: "STANDARD".to_string(),
        ..Default::default()
    };

    // Find the account that owns the destination bucket
    let dest_acct = state
        .read()
        .find_account(|s| s.buckets.contains_key(&dest_bucket_name))
        .map(|a| a.to_string());
    let Some(dest_acct) = dest_acct else {
        return;
    };
    if let Err(err) = crate::delivery::put_delivered_object(
        state,
        store,
        &dest_acct,
        &dest_bucket_name,
        report_object,
        data,
    ) {
        tracing::error!(
            bucket = %dest_bucket_name,
            key = %report_key,
            error = %err,
            "failed to deliver S3 inventory report"
        );
    }
}

/// Escape a value for inclusion in a CSV field.  If the value contains a
/// comma, double-quote, or newline it is wrapped in double quotes and any
/// embedded double quotes are doubled.
fn csv_escape(value: &str) -> String {
    if value.contains(',') || value.contains('"') || value.contains('\n') {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        format!("\"{value}\"")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_destination_from_inventory_config() {
        let xml = r#"<InventoryConfiguration>
            <Id>my-inv</Id>
            <Destination>
                <S3BucketDestination>
                    <Bucket>arn:aws:s3:::dest-bucket</Bucket>
                    <Format>CSV</Format>
                    <Prefix>inventory/</Prefix>
                </S3BucketDestination>
            </Destination>
            <IsEnabled>true</IsEnabled>
            <Schedule><Frequency>Daily</Frequency></Schedule>
            <IncludedObjectVersions>Current</IncludedObjectVersions>
        </InventoryConfiguration>"#;

        let dest = parse_inventory_destination(xml).unwrap();
        assert_eq!(dest.bucket_arn, "arn:aws:s3:::dest-bucket");
        assert_eq!(dest.prefix.as_deref(), Some("inventory/"));
    }

    #[test]
    fn bucket_name_from_arn_works() {
        assert_eq!(
            bucket_name_from_arn("arn:aws:s3:::my-bucket"),
            Some("my-bucket")
        );
        assert_eq!(bucket_name_from_arn("not-an-arn"), None);
    }

    #[test]
    fn csv_escape_plain_value() {
        assert_eq!(csv_escape("hello"), "\"hello\"");
    }

    #[test]
    fn csv_escape_value_with_comma() {
        assert_eq!(csv_escape("a,b"), "\"a,b\"");
    }

    #[test]
    fn csv_escape_value_with_quotes() {
        assert_eq!(csv_escape("say \"hi\""), "\"say \"\"hi\"\"\"");
    }

    #[test]
    fn csv_escape_value_with_comma_and_quotes() {
        assert_eq!(csv_escape("a,\"b\""), "\"a,\"\"b\"\"\"");
    }

    #[test]
    fn parse_inventory_missing_destination_block_is_none() {
        let xml = "<InventoryConfiguration></InventoryConfiguration>";
        assert!(parse_inventory_destination(xml).is_none());
    }

    #[test]
    fn parse_inventory_missing_bucket_returns_none() {
        let xml = "<InventoryConfiguration>
            <Destination>
                <S3BucketDestination>
                    <Format>CSV</Format>
                </S3BucketDestination>
            </Destination>
        </InventoryConfiguration>";
        assert!(parse_inventory_destination(xml).is_none());
    }

    // Inverted/duplicated tags (closing before opening) used to slice with
    // begin > end and panic (a reachable DoS under eager delivery); each guard
    // must return None instead.
    #[test]
    fn parse_inventory_inverted_tags_does_not_panic() {
        assert!(parse_inventory_destination("</Destination>x<Destination>").is_none());
        let inverted_inner =
            "<Destination></S3BucketDestination>y<S3BucketDestination></Destination>";
        assert!(parse_inventory_destination(inverted_inner).is_none());
    }

    #[test]
    fn parse_inventory_without_prefix_field_is_ok() {
        let xml = "<InventoryConfiguration>
            <Destination>
                <S3BucketDestination>
                    <Bucket>arn:aws:s3:::dest</Bucket>
                </S3BucketDestination>
            </Destination>
        </InventoryConfiguration>";
        let dest = parse_inventory_destination(xml).unwrap();
        assert_eq!(dest.bucket_arn, "arn:aws:s3:::dest");
        assert!(dest.prefix.is_none());
    }

    /// A source bucket with one object and an inventory configuration `cfg`
    /// delivering to `dest`.
    fn inventory_fixture(dest: crate::state::S3Bucket) -> SharedS3State {
        use crate::state::{memory_body, S3Bucket, S3Object, S3State};
        use fakecloud_core::multi_account::MultiAccountState;
        use parking_lot::RwLock;
        use std::sync::Arc;

        let mut s = S3State::new("123456789012", "us-east-1");
        let mut src = S3Bucket::new("src", "us-east-1", "owner");
        src.objects.insert(
            "file.txt".to_string(),
            S3Object {
                key: "file.txt".to_string(),
                body: memory_body(Bytes::from_static(b"abc")),
                content_type: "text/plain".to_string(),
                etag: "abc".to_string(),
                size: 3,
                last_modified: Utc::now(),
                storage_class: "STANDARD".to_string(),
                ..Default::default()
            },
        );
        src.inventory_configs.insert(
            "cfg".to_string(),
            r#"<InventoryConfiguration>
                <Destination>
                    <S3BucketDestination>
                        <Bucket>arn:aws:s3:::dest</Bucket>
                        <Prefix>inv/</Prefix>
                    </S3BucketDestination>
                </Destination>
            </InventoryConfiguration>"#
                .to_string(),
        );
        s.buckets.insert("src".to_string(), src);
        s.buckets.insert("dest".to_string(), dest);

        let mut multi: MultiAccountState<S3State> =
            MultiAccountState::new("123456789012", "us-east-1", "http://x");
        *multi.default_mut() = s;
        Arc::new(RwLock::new(multi))
    }

    #[test]
    fn generate_inventory_writes_csv_report() {
        let shared = inventory_fixture(crate::state::S3Bucket::new("dest", "us-east-1", "owner"));

        generate_inventory_report(&shared, None, "src", "cfg");

        let guard = shared.read();
        let dest = guard.default_ref().buckets.get("dest").unwrap();
        assert_eq!(dest.objects.len(), 1);
        let (key, obj) = dest.objects.iter().next().unwrap();
        assert!(key.starts_with("inv/src/cfg/data/"));
        assert_eq!(obj.content_type, "text/csv");
        assert_eq!(obj.version_id, None);
    }

    /// The report is a new object version in a versioned destination, with
    /// the destination's default retention, like any object written to it.
    #[test]
    fn inventory_report_is_a_new_version_in_a_versioned_lock_bucket() {
        let mut dest = crate::state::S3Bucket::new("dest", "us-east-1", "owner");
        dest.versioning = Some("Enabled".to_string());
        dest.object_lock_config = Some(
            "<ObjectLockConfiguration><Rule><DefaultRetention><Mode>GOVERNANCE</Mode>\
             <Days>2</Days></DefaultRetention></Rule></ObjectLockConfiguration>"
                .to_string(),
        );
        let shared = inventory_fixture(dest);

        generate_inventory_report(&shared, None, "src", "cfg");

        let guard = shared.read();
        let dest = guard.default_ref().buckets.get("dest").unwrap();
        let (key, obj) = dest.objects.iter().next().unwrap();
        let vid = obj.version_id.clone().expect("a version id");
        assert_ne!(vid, "null");
        assert_eq!(obj.lock_mode.as_deref(), Some("GOVERNANCE"));
        assert!(obj.lock_retain_until.is_some());
        let versions = &dest.object_versions[key];
        assert_eq!(versions.len(), 1);
        assert_eq!(versions[0].version_id.as_deref(), Some(vid.as_str()));
    }

    #[test]
    fn generate_inventory_missing_config_is_noop() {
        use crate::state::{S3Bucket, S3State};
        use fakecloud_core::multi_account::MultiAccountState;
        use parking_lot::RwLock;
        use std::sync::Arc;

        let mut s = S3State::new("123456789012", "us-east-1");
        s.buckets.insert(
            "src".to_string(),
            S3Bucket::new("src", "us-east-1", "owner"),
        );
        let mut multi: MultiAccountState<S3State> =
            MultiAccountState::new("123456789012", "us-east-1", "http://x");
        *multi.default_mut() = s;
        let shared: SharedS3State = Arc::new(RwLock::new(multi));

        // Should not panic even though cfg doesn't exist
        generate_inventory_report(&shared, None, "src", "missing");
    }
}
