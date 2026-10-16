use aws_sdk_firehose::primitives::Blob;
use aws_sdk_firehose::types::{
    BufferingHints, DeliveryStreamType, ExtendedS3DestinationConfiguration,
    KinesisStreamSourceConfiguration, Record,
};
use aws_sdk_s3::types::CreateBucketConfiguration;
use fakecloud_testkit::TestServer;

async fn setup(server: &TestServer, bucket: &str) -> aws_sdk_firehose::Client {
    let s3 = server.s3_client().await;
    let _ = s3
        .create_bucket()
        .bucket(bucket)
        .create_bucket_configuration(
            CreateBucketConfiguration::builder()
                .location_constraint(aws_sdk_s3::types::BucketLocationConstraint::UsWest2)
                .build(),
        )
        .send()
        .await;
    server.firehose_client().await
}

fn ext_s3(bucket_arn: &str) -> ExtendedS3DestinationConfiguration {
    ExtendedS3DestinationConfiguration::builder()
        .role_arn("arn:aws:iam::123456789012:role/firehose")
        .bucket_arn(bucket_arn)
        .buffering_hints(
            BufferingHints::builder()
                .size_in_mbs(5)
                .interval_in_seconds(60)
                .build(),
        )
        .build()
        .unwrap()
}

fn assert_kinesis_source_error<E>(err: aws_sdk_firehose::error::SdkError<E>)
where
    E: std::fmt::Debug + aws_sdk_firehose::error::ProvideErrorMetadata,
{
    let aws_sdk_firehose::error::SdkError::ServiceError(context) = err else {
        panic!("expected an AWS service error, got {err:?}");
    };
    assert_eq!(context.raw().status().as_u16(), 400);
    assert_eq!(context.err().code(), Some("InvalidArgumentException"));
    assert_eq!(
        context.err().message(),
        Some("This operation is not permitted on KinesisStreamAsSource delivery stream type.")
    );
}

#[tokio::test]
async fn firehose_kinesis_source_rejects_puts_and_direct_put_still_delivers() {
    tokio::time::timeout(std::time::Duration::from_secs(60), async {
        let server = TestServer::start().await;
        let firehose = setup(&server, "fh-source-admission").await;
        let s3 = server.s3_client().await;
        let kinesis = server.kinesis_client().await;
        kinesis
            .create_stream()
            .stream_name("fh-source")
            .shard_count(1)
            .send()
            .await
            .expect("create Kinesis source");
        let source = kinesis
            .describe_stream_summary()
            .stream_name("fh-source")
            .send()
            .await
            .expect("describe Kinesis source");
        firehose
            .create_delivery_stream()
            .delivery_stream_name("fh-source-stream")
            .delivery_stream_type(DeliveryStreamType::KinesisStreamAsSource)
            .kinesis_stream_source_configuration(
                KinesisStreamSourceConfiguration::builder()
                    .kinesis_stream_arn(source.stream_description_summary().unwrap().stream_arn())
                    .role_arn("arn:aws:iam::123456789012:role/firehose")
                    .build()
                    .unwrap(),
            )
            .extended_s3_destination_configuration(ext_s3("arn:aws:s3:::fh-source-admission"))
            .send()
            .await
            .expect("create Kinesis-source Firehose stream");
        let description = firehose
            .describe_delivery_stream()
            .delivery_stream_name("fh-source-stream")
            .send()
            .await
            .expect("describe source delivery stream");
        assert_eq!(
            description
                .delivery_stream_description()
                .unwrap()
                .delivery_stream_type(),
            &DeliveryStreamType::KinesisStreamAsSource
        );

        let record = |data: &[u8]| Record::builder().data(Blob::new(data)).build().unwrap();
        assert_kinesis_source_error(
            firehose
                .put_record()
                .delivery_stream_name("fh-source-stream")
                .record(record(b"rejected-single"))
                .send()
                .await
                .expect_err("source stream rejects PutRecord"),
        );
        assert_kinesis_source_error(
            firehose
                .put_record_batch()
                .delivery_stream_name("fh-source-stream")
                .records(record(b"rejected-a"))
                .records(record(b"rejected-b"))
                .send()
                .await
                .expect_err("source stream rejects PutRecordBatch"),
        );
        let listing = s3
            .list_objects_v2()
            .bucket("fh-source-admission")
            .send()
            .await
            .expect("list after rejected puts");
        assert!(
            listing.contents().is_empty(),
            "rejected puts must not write S3"
        );

        firehose
            .create_delivery_stream()
            .delivery_stream_name("fh-direct-control")
            .delivery_stream_type(DeliveryStreamType::DirectPut)
            .extended_s3_destination_configuration(ext_s3("arn:aws:s3:::fh-source-admission"))
            .send()
            .await
            .expect("create DirectPut control");
        let single = firehose
            .put_record()
            .delivery_stream_name("fh-direct-control")
            .record(record(b"direct-single"))
            .send()
            .await
            .expect("DirectPut PutRecord succeeds");
        assert!(!single.record_id().is_empty());
        let batch = firehose
            .put_record_batch()
            .delivery_stream_name("fh-direct-control")
            .records(record(b"direct-a"))
            .records(record(b"direct-b"))
            .send()
            .await
            .expect("DirectPut PutRecordBatch succeeds");
        assert_eq!(batch.failed_put_count(), 0);
        assert_eq!(batch.request_responses().len(), 2);
        assert!(batch
            .request_responses()
            .iter()
            .all(|r| r.record_id().is_some_and(|id| !id.is_empty())));

        let listing = s3
            .list_objects_v2()
            .bucket("fh-source-admission")
            .send()
            .await
            .expect("list DirectPut deliveries");
        assert_eq!(listing.contents().len(), 2);
        let mut payloads = Vec::new();
        for object in listing.contents() {
            let delivered = s3
                .get_object()
                .bucket("fh-source-admission")
                .key(object.key().unwrap())
                .send()
                .await
                .expect("read DirectPut object");
            payloads.push(
                delivered
                    .body
                    .collect()
                    .await
                    .expect("collect DirectPut bytes")
                    .into_bytes()
                    .to_vec(),
            );
        }
        payloads.sort();
        assert_eq!(
            payloads,
            [
                b"direct-a\ndirect-b\n".to_vec(),
                b"direct-single\n".to_vec()
            ]
        );
    })
    .await
    .expect("Firehose source-admission E2E timed out");
}

#[tokio::test]
async fn firehose_create_describe_delete_roundtrip() {
    let server = TestServer::start().await;
    let firehose = setup(&server, "fh-bucket-create").await;
    let bucket_arn = "arn:aws:s3:::fh-bucket-create";

    let create = firehose
        .create_delivery_stream()
        .delivery_stream_name("fh-stream-1")
        .extended_s3_destination_configuration(ext_s3(bucket_arn))
        .send()
        .await
        .expect("create");
    assert!(create
        .delivery_stream_arn()
        .unwrap()
        .ends_with(":deliverystream/fh-stream-1"));

    let desc = firehose
        .describe_delivery_stream()
        .delivery_stream_name("fh-stream-1")
        .send()
        .await
        .expect("describe");
    let d = desc.delivery_stream_description().unwrap();
    assert_eq!(d.delivery_stream_name(), "fh-stream-1");
    assert_eq!(d.delivery_stream_status().as_str(), "ACTIVE");
    // AWS always reports the ExtendedS3 defaults the caller omitted: the
    // CloudWatch logging block (disabled), custom_time_zone=UTC, and
    // s3_backup_mode=Disabled.
    let ext = d.destinations()[0]
        .extended_s3_destination_description()
        .expect("extended s3 description");
    let cwl = ext
        .cloud_watch_logging_options()
        .expect("cloudwatch logging options");
    assert_eq!(cwl.enabled(), Some(false));
    assert_eq!(ext.custom_time_zone(), Some("UTC"));
    assert_eq!(
        ext.s3_backup_mode(),
        Some(&aws_sdk_firehose::types::S3BackupMode::Disabled)
    );

    let listed = firehose.list_delivery_streams().send().await.expect("list");
    assert!(listed
        .delivery_stream_names()
        .iter()
        .any(|n| n == "fh-stream-1"));

    firehose
        .delete_delivery_stream()
        .delivery_stream_name("fh-stream-1")
        .send()
        .await
        .expect("delete");

    let err = firehose
        .describe_delivery_stream()
        .delivery_stream_name("fh-stream-1")
        .send()
        .await;
    assert!(err.is_err());
}

#[tokio::test]
async fn firehose_http_endpoint_destination_round_trips() {
    use aws_sdk_firehose::types::{
        HttpEndpointConfiguration, HttpEndpointDestinationConfiguration,
        HttpEndpointDestinationUpdate,
    };

    let server = TestServer::start().await;
    let firehose = setup(&server, "fh-http-bucket").await;

    // A non-S3 (HTTP endpoint) destination must round-trip through
    // DescribeDeliveryStream instead of being dropped (bug-hunt 2026-06-24,
    // 1.13 — UpdateDestination/Create only handled S3/ExtendedS3).
    firehose
        .create_delivery_stream()
        .delivery_stream_name("fh-http")
        .http_endpoint_destination_configuration(
            HttpEndpointDestinationConfiguration::builder()
                .endpoint_configuration(
                    HttpEndpointConfiguration::builder()
                        .url("https://example.com/ingest")
                        .name("my-endpoint")
                        .build()
                        .unwrap(),
                )
                .role_arn("arn:aws:iam::123456789012:role/firehose")
                .build(),
        )
        .send()
        .await
        .expect("create http endpoint stream");

    let desc = firehose
        .describe_delivery_stream()
        .delivery_stream_name("fh-http")
        .send()
        .await
        .expect("describe");
    let d = desc.delivery_stream_description().unwrap();
    let http = d.destinations()[0]
        .http_endpoint_destination_description()
        .expect("http endpoint description present");
    assert_eq!(
        http.endpoint_configuration().and_then(|e| e.url()),
        Some("https://example.com/ingest")
    );

    // UpdateDestination must also persist a changed HTTP endpoint.
    let version = d.version_id();
    let dest_id = d.destinations()[0].destination_id();
    firehose
        .update_destination()
        .delivery_stream_name("fh-http")
        .current_delivery_stream_version_id(version)
        .destination_id(dest_id)
        .http_endpoint_destination_update(
            HttpEndpointDestinationUpdate::builder()
                .endpoint_configuration(
                    HttpEndpointConfiguration::builder()
                        .url("https://example.com/v2")
                        .build()
                        .unwrap(),
                )
                .build(),
        )
        .send()
        .await
        .expect("update http endpoint");

    let desc = firehose
        .describe_delivery_stream()
        .delivery_stream_name("fh-http")
        .send()
        .await
        .expect("describe after update");
    let http = desc.delivery_stream_description().unwrap().destinations()[0]
        .http_endpoint_destination_description()
        .expect("http endpoint description after update");
    assert_eq!(
        http.endpoint_configuration().and_then(|e| e.url()),
        Some("https://example.com/v2")
    );
}

#[tokio::test]
async fn firehose_put_record_writes_to_s3() {
    let server = TestServer::start().await;
    let firehose = setup(&server, "fh-bucket-put").await;
    let bucket_arn = "arn:aws:s3:::fh-bucket-put";

    firehose
        .create_delivery_stream()
        .delivery_stream_name("fh-put-stream")
        .extended_s3_destination_configuration(ext_s3(bucket_arn))
        .send()
        .await
        .expect("create");

    firehose
        .put_record()
        .delivery_stream_name("fh-put-stream")
        .record(
            Record::builder()
                .data(Blob::new(b"hello-firehose"))
                .build()
                .unwrap(),
        )
        .send()
        .await
        .expect("put record");

    let s3 = server.s3_client().await;
    let listing = s3
        .list_objects_v2()
        .bucket("fh-bucket-put")
        .send()
        .await
        .expect("list");
    let key = listing
        .contents()
        .first()
        .expect("at least one object")
        .key()
        .unwrap()
        .to_string();
    let obj = s3
        .get_object()
        .bucket("fh-bucket-put")
        .key(&key)
        .send()
        .await
        .expect("get");
    let bytes = obj.body.collect().await.expect("collect").into_bytes();
    assert!(bytes.starts_with(b"hello-firehose"));
}

#[tokio::test]
async fn firehose_put_record_batch_aggregates_payloads() {
    let server = TestServer::start().await;
    let firehose = setup(&server, "fh-bucket-batch").await;
    let bucket_arn = "arn:aws:s3:::fh-bucket-batch";

    firehose
        .create_delivery_stream()
        .delivery_stream_name("fh-batch-stream")
        .extended_s3_destination_configuration(ext_s3(bucket_arn))
        .send()
        .await
        .expect("create");

    let resp = firehose
        .put_record_batch()
        .delivery_stream_name("fh-batch-stream")
        .records(
            Record::builder()
                .data(Blob::new(b"line-a"))
                .build()
                .unwrap(),
        )
        .records(
            Record::builder()
                .data(Blob::new(b"line-b"))
                .build()
                .unwrap(),
        )
        .records(
            Record::builder()
                .data(Blob::new(b"line-c"))
                .build()
                .unwrap(),
        )
        .send()
        .await
        .expect("put batch");
    assert_eq!(resp.failed_put_count(), 0);
    assert_eq!(resp.request_responses().len(), 3);

    let s3 = server.s3_client().await;
    let listing = s3
        .list_objects_v2()
        .bucket("fh-bucket-batch")
        .send()
        .await
        .expect("list");
    let key = listing
        .contents()
        .first()
        .unwrap()
        .key()
        .unwrap()
        .to_string();
    let obj = s3
        .get_object()
        .bucket("fh-bucket-batch")
        .key(&key)
        .send()
        .await
        .expect("get");
    let bytes = obj.body.collect().await.expect("collect").into_bytes();
    let s = std::str::from_utf8(&bytes).unwrap();
    assert!(s.contains("line-a"));
    assert!(s.contains("line-b"));
    assert!(s.contains("line-c"));
}

#[tokio::test]
async fn firehose_put_record_batch_reports_invalid_records() {
    let server = TestServer::start().await;
    let firehose = setup(&server, "fh-bucket-invalid").await;
    let bucket_arn = "arn:aws:s3:::fh-bucket-invalid";

    firehose
        .create_delivery_stream()
        .delivery_stream_name("fh-invalid-stream")
        .extended_s3_destination_configuration(ext_s3(bucket_arn))
        .send()
        .await
        .expect("create");

    // Mix valid + valid + valid records (SDK base64-encodes Blob for us, so
    // we can't construct invalid base64 via the SDK). Validate happy-path
    // counts here; invalid-record handling is exercised at the wire level
    // below by talking directly to the JSON endpoint.
    let resp = firehose
        .put_record_batch()
        .delivery_stream_name("fh-invalid-stream")
        .records(Record::builder().data(Blob::new(b"ok-1")).build().unwrap())
        .records(Record::builder().data(Blob::new(b"ok-2")).build().unwrap())
        .send()
        .await
        .expect("put batch");
    assert_eq!(resp.failed_put_count(), 0);

    // Wire-level invalid-base64 records should bump FailedPutCount and
    // surface an ErrorCode on the corresponding response entry.
    let http = reqwest::Client::new();
    let url = format!("{}/", server.endpoint());
    let body = serde_json::json!({
        "DeliveryStreamName": "fh-invalid-stream",
        "Records": [
            {"Data": "@@@not-base64@@@"},
            {"Data": "Z29vZA=="}
        ]
    });
    let r = http
        .post(&url)
        .header("Content-Type", "application/x-amz-json-1.1")
        .header("X-Amz-Target", "Firehose_20150804.PutRecordBatch")
        .header(
            "Authorization",
            "AWS4-HMAC-SHA256 \
             Credential=AKIAIOSFODNN7EXAMPLE/20260501/us-east-1/firehose/aws4_request, \
             SignedHeaders=host;x-amz-target, Signature=deadbeef",
        )
        .body(body.to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let json: serde_json::Value = r.json().await.unwrap();
    assert_eq!(json["FailedPutCount"], 1);
    let resps = json["RequestResponses"].as_array().unwrap();
    assert_eq!(resps.len(), 2);
    assert_eq!(resps[0]["ErrorCode"], "InvalidArgumentException");
    assert!(resps[1].get("RecordId").is_some());
}

#[tokio::test]
async fn firehose_streams_region_isolated() {
    use aws_config::BehaviorVersion;
    use aws_credential_types::Credentials;
    use aws_sdk_firehose::config::Region;

    let server = TestServer::start().await;
    let _ = setup(&server, "fh-bucket-region").await;

    fn config_for(server: &TestServer, region: &str) -> aws_sdk_firehose::Config {
        let creds = Credentials::new(
            "AKIAIOSFODNN7EXAMPLE",
            "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            None,
            None,
            "test",
        );
        aws_sdk_firehose::Config::builder()
            .behavior_version(BehaviorVersion::latest())
            .endpoint_url(server.endpoint())
            .region(Region::new(region.to_string()))
            .credentials_provider(creds)
            .build()
    }

    let east = aws_sdk_firehose::Client::from_conf(config_for(&server, "us-east-1"));
    let west = aws_sdk_firehose::Client::from_conf(config_for(&server, "us-west-2"));

    let bucket_arn = "arn:aws:s3:::fh-bucket-region";
    east.create_delivery_stream()
        .delivery_stream_name("regional-stream")
        .extended_s3_destination_configuration(ext_s3(bucket_arn))
        .send()
        .await
        .expect("create east");

    let east_desc = east
        .describe_delivery_stream()
        .delivery_stream_name("regional-stream")
        .send()
        .await
        .expect("describe east");
    assert_eq!(
        east_desc
            .delivery_stream_description()
            .unwrap()
            .delivery_stream_name(),
        "regional-stream"
    );

    let west_err = west
        .describe_delivery_stream()
        .delivery_stream_name("regional-stream")
        .send()
        .await;
    assert!(
        west_err.is_err(),
        "stream from us-east-1 must not be visible in us-west-2"
    );

    let west_listed = west
        .list_delivery_streams()
        .send()
        .await
        .expect("list west");
    assert!(west_listed.delivery_stream_names().is_empty());
}

#[tokio::test]
async fn firehose_tags_roundtrip() {
    let server = TestServer::start().await;
    let firehose = setup(&server, "fh-bucket-tags").await;
    let bucket_arn = "arn:aws:s3:::fh-bucket-tags";

    firehose
        .create_delivery_stream()
        .delivery_stream_name("fh-tags-stream")
        .extended_s3_destination_configuration(ext_s3(bucket_arn))
        .send()
        .await
        .expect("create");

    firehose
        .tag_delivery_stream()
        .delivery_stream_name("fh-tags-stream")
        .tags(
            aws_sdk_firehose::types::Tag::builder()
                .key("env")
                .value("prod")
                .build()
                .unwrap(),
        )
        .send()
        .await
        .expect("tag");

    let listed = firehose
        .list_tags_for_delivery_stream()
        .delivery_stream_name("fh-tags-stream")
        .send()
        .await
        .expect("list tags");
    assert_eq!(listed.tags().len(), 1);
    assert_eq!(listed.tags()[0].key(), "env");

    firehose
        .untag_delivery_stream()
        .delivery_stream_name("fh-tags-stream")
        .tag_keys("env")
        .send()
        .await
        .expect("untag");
    let listed = firehose
        .list_tags_for_delivery_stream()
        .delivery_stream_name("fh-tags-stream")
        .send()
        .await
        .expect("list tags");
    assert_eq!(listed.tags().len(), 0);
}

#[tokio::test]
async fn firehose_create_with_encryption_reflected_in_describe() {
    use aws_sdk_firehose::types::{DeliveryStreamEncryptionConfigurationInput, KeyType};
    let server = TestServer::start().await;
    let firehose = setup(&server, "fh-bucket-enc").await;
    let key = "arn:aws:kms:us-east-1:123456789012:key/1234abcd";

    firehose
        .create_delivery_stream()
        .delivery_stream_name("fh-enc")
        .extended_s3_destination_configuration(ext_s3("arn:aws:s3:::fh-bucket-enc"))
        .delivery_stream_encryption_configuration_input(
            DeliveryStreamEncryptionConfigurationInput::builder()
                .key_type(KeyType::CustomerManagedCmk)
                .key_arn(key)
                .build()
                .unwrap(),
        )
        .send()
        .await
        .expect("create");
    let desc = firehose
        .describe_delivery_stream()
        .delivery_stream_name("fh-enc")
        .send()
        .await
        .expect("describe");
    let enc = desc
        .delivery_stream_description()
        .unwrap()
        .delivery_stream_encryption_configuration()
        .expect("encryption configuration");
    assert_eq!(enc.status().map(|s| s.as_str()), Some("ENABLED"));
    assert_eq!(enc.key_type(), Some(&KeyType::CustomerManagedCmk));
    assert_eq!(enc.key_arn(), Some(key));
}

/// A delivered batch is an ordinary new object version in the destination
/// bucket: on a versioned Object Lock bucket each delivery gets a real version
/// id (visible in ListObjectVersions) and the bucket's default retention.
#[tokio::test]
async fn firehose_delivery_to_versioned_object_lock_bucket() {
    use aws_sdk_s3::types::{
        DefaultRetention, ObjectLockConfiguration, ObjectLockEnabled, ObjectLockRetentionMode,
        ObjectLockRule,
    };

    let server = TestServer::start().await;
    let s3 = server.s3_client().await;
    let bucket = "fh-locked-bucket";
    s3.create_bucket()
        .bucket(bucket)
        .object_lock_enabled_for_bucket(true)
        .send()
        .await
        .expect("create bucket");
    s3.put_object_lock_configuration()
        .bucket(bucket)
        .object_lock_configuration(
            ObjectLockConfiguration::builder()
                .object_lock_enabled(ObjectLockEnabled::Enabled)
                .rule(
                    ObjectLockRule::builder()
                        .default_retention(
                            DefaultRetention::builder()
                                .mode(ObjectLockRetentionMode::Governance)
                                .days(2)
                                .build(),
                        )
                        .build(),
                )
                .build(),
        )
        .send()
        .await
        .expect("put object lock configuration");

    let firehose = server.firehose_client().await;
    firehose
        .create_delivery_stream()
        .delivery_stream_name("fh-locked-stream")
        .extended_s3_destination_configuration(ext_s3(&format!("arn:aws:s3:::{bucket}")))
        .send()
        .await
        .expect("create");
    let before = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    for payload in [&b"first"[..], &b"second"[..]] {
        firehose
            .put_record()
            .delivery_stream_name("fh-locked-stream")
            .record(Record::builder().data(Blob::new(payload)).build().unwrap())
            .send()
            .await
            .expect("put record");
    }

    let versions = s3
        .list_object_versions()
        .bucket(bucket)
        .send()
        .await
        .expect("list object versions");
    let versions = versions.versions();
    assert_eq!(versions.len(), 2, "{versions:?}");
    let two_days = 2 * 24 * 60 * 60;
    for v in versions {
        let key = v.key().unwrap();
        let vid = v.version_id().expect("version id");
        assert_ne!(vid, "null", "delivered object must get a real version id");
        assert_eq!(v.is_latest(), Some(true));

        let head = s3
            .head_object()
            .bucket(bucket)
            .key(key)
            .send()
            .await
            .expect("head");
        assert_eq!(head.version_id(), Some(vid));

        let retention = s3
            .get_object_retention()
            .bucket(bucket)
            .key(key)
            .version_id(vid)
            .send()
            .await
            .expect("get object retention")
            .retention()
            .cloned()
            .expect("default retention applied");
        assert_eq!(retention.mode(), Some(&ObjectLockRetentionMode::Governance));
        let until = retention.retain_until_date().expect("retain until").secs();
        assert!(
            until >= before + two_days - 5 && until <= before + two_days + 120,
            "retain-until {until} is not ~2 days after {before}"
        );
    }
}
