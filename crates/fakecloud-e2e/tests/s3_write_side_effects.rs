//! Every way an object lands in a bucket gets the same S3 side effects as a
//! PutObject: replication, event notifications and bucket default encryption.
//! Covers CompleteMultipartUpload and objects another service delivers
//! (Firehose here), plus the replication rules S3 enforces around them.

use aws_sdk_firehose::primitives::Blob;
use aws_sdk_firehose::types::{BufferingHints, ExtendedS3DestinationConfiguration, Record};
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::{
    BucketVersioningStatus, CompletedMultipartUpload, CompletedPart, DeleteMarkerReplication,
    DeleteMarkerReplicationStatus, Destination, Event, NotificationConfiguration,
    QueueConfiguration, ReplicationConfiguration, ReplicationRule, ReplicationRuleFilter,
    ReplicationRuleStatus, ReplicationStatus, ServerSideEncryption, ServerSideEncryptionByDefault,
    ServerSideEncryptionConfiguration, ServerSideEncryptionRule, VersioningConfiguration,
};
use fakecloud_testkit::TestServer;

async fn versioned_bucket(s3: &aws_sdk_s3::Client, bucket: &str) {
    s3.create_bucket().bucket(bucket).send().await.unwrap();
    s3.put_bucket_versioning()
        .bucket(bucket)
        .versioning_configuration(
            VersioningConfiguration::builder()
                .status(BucketVersioningStatus::Enabled)
                .build(),
        )
        .send()
        .await
        .unwrap();
}

fn rule(
    id: &str,
    priority: i32,
    prefix: &str,
    dest: &str,
    delete_markers: bool,
) -> ReplicationRule {
    ReplicationRule::builder()
        .id(id)
        .priority(priority)
        .status(ReplicationRuleStatus::Enabled)
        .filter(ReplicationRuleFilter::builder().prefix(prefix).build())
        .delete_marker_replication(
            DeleteMarkerReplication::builder()
                .status(if delete_markers {
                    DeleteMarkerReplicationStatus::Enabled
                } else {
                    DeleteMarkerReplicationStatus::Disabled
                })
                .build(),
        )
        .destination(
            Destination::builder()
                .bucket(format!("arn:aws:s3:::{dest}"))
                .build()
                .unwrap(),
        )
        .build()
        .unwrap()
}

/// PutBucketReplication with `rules`; the error code when S3 refuses it.
async fn put_replication(
    s3: &aws_sdk_s3::Client,
    bucket: &str,
    rules: Vec<ReplicationRule>,
) -> Result<(), String> {
    use aws_sdk_s3::error::ProvideErrorMetadata;
    s3.put_bucket_replication()
        .bucket(bucket)
        .replication_configuration(
            ReplicationConfiguration::builder()
                .role("arn:aws:iam::123456789012:role/replication")
                .set_rules(Some(rules))
                .build()
                .unwrap(),
        )
        .send()
        .await
        .map(|_| ())
        .map_err(|e| e.code().unwrap_or("unknown").to_string())
}

/// A queue receiving every `s3:ObjectCreated:*` event of `bucket`; returns
/// its URL.
async fn wire_created_events_to_queue(
    s3: &aws_sdk_s3::Client,
    sqs: &aws_sdk_sqs::Client,
    bucket: &str,
    queue: &str,
) -> String {
    let url = sqs
        .create_queue()
        .queue_name(queue)
        .send()
        .await
        .unwrap()
        .queue_url()
        .unwrap()
        .to_string();
    s3.put_bucket_notification_configuration()
        .bucket(bucket)
        .notification_configuration(
            NotificationConfiguration::builder()
                .queue_configurations(
                    QueueConfiguration::builder()
                        .id("created")
                        .queue_arn(format!("arn:aws:sqs:us-east-1:123456789012:{queue}"))
                        .events(Event::S3ObjectCreated)
                        .build()
                        .unwrap(),
                )
                .build(),
        )
        .send()
        .await
        .unwrap();
    url
}

async fn firehose_to(server: &TestServer, stream: &str, bucket: &str) -> aws_sdk_firehose::Client {
    let firehose = server.firehose_client().await;
    firehose
        .create_delivery_stream()
        .delivery_stream_name(stream)
        .extended_s3_destination_configuration(
            ExtendedS3DestinationConfiguration::builder()
                .role_arn("arn:aws:iam::123456789012:role/firehose")
                .bucket_arn(format!("arn:aws:s3:::{bucket}"))
                .prefix("delivered/")
                .buffering_hints(
                    BufferingHints::builder()
                        .size_in_mbs(5)
                        .interval_in_seconds(60)
                        .build(),
                )
                .build()
                .unwrap(),
        )
        .send()
        .await
        .unwrap();
    firehose
}

async fn put_record(firehose: &aws_sdk_firehose::Client, stream: &str, data: &[u8]) {
    firehose
        .put_record()
        .delivery_stream_name(stream)
        .record(Record::builder().data(Blob::new(data)).build().unwrap())
        .send()
        .await
        .unwrap();
}

async fn only_key(s3: &aws_sdk_s3::Client, bucket: &str) -> String {
    let listing = s3.list_objects_v2().bucket(bucket).send().await.unwrap();
    let contents = listing.contents();
    assert_eq!(contents.len(), 1, "{contents:?}");
    contents[0].key().unwrap().to_string()
}

/// CompleteMultipartUpload on a replication source replicates the object
/// like PutObject: the replica carries the source version id and multipart
/// ETag and reports REPLICA, the source reports COMPLETED.
#[tokio::test]
async fn multipart_upload_is_replicated() {
    let server = TestServer::start().await;
    let s3 = server.s3_client().await;
    versioned_bucket(&s3, "mpu-repl-src").await;
    versioned_bucket(&s3, "mpu-repl-dst").await;
    put_replication(
        &s3,
        "mpu-repl-src",
        vec![rule("all", 1, "", "mpu-repl-dst", false)],
    )
    .await
    .unwrap();

    let upload = s3
        .create_multipart_upload()
        .bucket("mpu-repl-src")
        .key("big/object.bin")
        .content_type("application/x-custom")
        .metadata("origin", "multipart")
        .send()
        .await
        .unwrap();
    let upload_id = upload.upload_id().unwrap();
    let part1 = vec![b'a'; 5 * 1024 * 1024];
    let mut parts = Vec::new();
    for (n, data) in [(1, part1.clone()), (2, b"last part".to_vec())] {
        let out = s3
            .upload_part()
            .bucket("mpu-repl-src")
            .key("big/object.bin")
            .upload_id(upload_id)
            .part_number(n)
            .body(ByteStream::from(data))
            .send()
            .await
            .unwrap();
        parts.push(
            CompletedPart::builder()
                .part_number(n)
                .e_tag(out.e_tag().unwrap())
                .build(),
        );
    }
    let done = s3
        .complete_multipart_upload()
        .bucket("mpu-repl-src")
        .key("big/object.bin")
        .upload_id(upload_id)
        .multipart_upload(
            CompletedMultipartUpload::builder()
                .set_parts(Some(parts))
                .build(),
        )
        .send()
        .await
        .unwrap();
    let version = done
        .version_id()
        .expect("versioned source reports a version");

    let source = s3
        .head_object()
        .bucket("mpu-repl-src")
        .key("big/object.bin")
        .send()
        .await
        .unwrap();
    assert_eq!(
        source.replication_status(),
        Some(&ReplicationStatus::Completed)
    );

    let replica = s3
        .get_object()
        .bucket("mpu-repl-dst")
        .key("big/object.bin")
        .send()
        .await
        .expect("the completed upload is replicated");
    assert_eq!(
        replica.replication_status(),
        Some(&ReplicationStatus::Replica)
    );
    assert_eq!(replica.version_id(), Some(version));
    assert_eq!(replica.e_tag(), done.e_tag());
    assert!(replica.e_tag().unwrap().ends_with("-2\""));
    assert_eq!(replica.content_type(), Some("application/x-custom"));
    assert_eq!(
        replica
            .metadata()
            .and_then(|m| m.get("origin"))
            .map(String::as_str),
        Some("multipart")
    );
    let body = replica.body.collect().await.unwrap().into_bytes();
    assert_eq!(body.len(), part1.len() + b"last part".len());
    assert!(body.ends_with(b"last part"));
}

/// An object Firehose delivers is an ordinary new object: it fires the
/// bucket's `ObjectCreated:Put` notification and is replicated.
#[tokio::test]
async fn firehose_delivery_fires_notification_and_is_replicated() {
    let server = TestServer::start().await;
    let s3 = server.s3_client().await;
    let sqs = server.sqs_client().await;
    versioned_bucket(&s3, "fh-fx-src").await;
    versioned_bucket(&s3, "fh-fx-dst").await;
    put_replication(
        &s3,
        "fh-fx-src",
        vec![rule("all", 1, "", "fh-fx-dst", false)],
    )
    .await
    .unwrap();
    let queue_url = wire_created_events_to_queue(&s3, &sqs, "fh-fx-src", "fh-fx-events").await;

    let firehose = firehose_to(&server, "fh-fx-stream", "fh-fx-src").await;
    put_record(&firehose, "fh-fx-stream", b"firehose-record").await;
    let key = only_key(&s3, "fh-fx-src").await;
    assert!(key.starts_with("delivered/"), "{key}");

    let messages = sqs
        .receive_message()
        .queue_url(&queue_url)
        .wait_time_seconds(2)
        .max_number_of_messages(10)
        .send()
        .await
        .unwrap();
    let messages = messages.messages();
    assert_eq!(messages.len(), 1, "{messages:?}");
    let event: serde_json::Value = serde_json::from_str(messages[0].body().unwrap()).unwrap();
    let record = &event["Records"][0];
    assert_eq!(record["eventSource"], "aws:s3");
    assert_eq!(record["eventName"], "ObjectCreated:Put");
    assert_eq!(record["s3"]["bucket"]["name"], "fh-fx-src");
    assert_eq!(record["s3"]["configurationId"], "created");
    assert_eq!(record["s3"]["object"]["size"], 15);
    let source = s3
        .head_object()
        .bucket("fh-fx-src")
        .key(&key)
        .send()
        .await
        .unwrap();
    assert_eq!(
        record["s3"]["object"]["versionId"].as_str(),
        source.version_id()
    );
    assert_eq!(
        source.replication_status(),
        Some(&ReplicationStatus::Completed)
    );

    let replica = s3
        .get_object()
        .bucket("fh-fx-dst")
        .key(&key)
        .send()
        .await
        .expect("the delivered object is replicated");
    assert_eq!(
        replica.replication_status(),
        Some(&ReplicationStatus::Replica)
    );
    assert_eq!(replica.version_id(), source.version_id());
    let body = replica.body.collect().await.unwrap().into_bytes();
    assert_eq!(body.as_ref(), b"firehose-record");
}

/// A bucket whose default encryption is SSE-KMS stores a delivered object
/// encrypted under the configured key: HeadObject reports `aws:kms` and the
/// key, and GetObject returns the plaintext.
#[tokio::test]
async fn delivered_object_gets_bucket_default_encryption() {
    let server = TestServer::start().await;
    let s3 = server.s3_client().await;
    let kms = server.kms_client().await;
    let key = kms.create_key().send().await.unwrap();
    let key_arn = key.key_metadata().unwrap().arn().unwrap().to_string();

    s3.create_bucket()
        .bucket("fh-sse-kms")
        .send()
        .await
        .unwrap();
    s3.put_bucket_encryption()
        .bucket("fh-sse-kms")
        .server_side_encryption_configuration(
            ServerSideEncryptionConfiguration::builder()
                .rules(
                    ServerSideEncryptionRule::builder()
                        .apply_server_side_encryption_by_default(
                            ServerSideEncryptionByDefault::builder()
                                .sse_algorithm(ServerSideEncryption::AwsKms)
                                .kms_master_key_id(&key_arn)
                                .build()
                                .unwrap(),
                        )
                        .build(),
                )
                .build()
                .unwrap(),
        )
        .send()
        .await
        .unwrap();

    let firehose = firehose_to(&server, "fh-sse-stream", "fh-sse-kms").await;
    put_record(&firehose, "fh-sse-stream", b"encrypted at rest").await;
    let object_key = only_key(&s3, "fh-sse-kms").await;

    let head = s3
        .head_object()
        .bucket("fh-sse-kms")
        .key(&object_key)
        .send()
        .await
        .unwrap();
    assert_eq!(
        head.server_side_encryption(),
        Some(&ServerSideEncryption::AwsKms)
    );
    assert_eq!(head.ssekms_key_id(), Some(key_arn.as_str()));
    assert_eq!(head.content_length(), Some(17));

    let got = s3
        .get_object()
        .bucket("fh-sse-kms")
        .key(&object_key)
        .send()
        .await
        .unwrap();
    let body = got.body.collect().await.unwrap().into_bytes();
    assert_eq!(body.as_ref(), b"encrypted at rest");

    // SSE-S3 default: the delivered object reports AES256.
    s3.create_bucket().bucket("fh-sse-s3").send().await.unwrap();
    s3.put_bucket_encryption()
        .bucket("fh-sse-s3")
        .server_side_encryption_configuration(
            ServerSideEncryptionConfiguration::builder()
                .rules(
                    ServerSideEncryptionRule::builder()
                        .apply_server_side_encryption_by_default(
                            ServerSideEncryptionByDefault::builder()
                                .sse_algorithm(ServerSideEncryption::Aes256)
                                .build()
                                .unwrap(),
                        )
                        .bucket_key_enabled(true)
                        .build(),
                )
                .build()
                .unwrap(),
        )
        .send()
        .await
        .unwrap();
    let firehose = firehose_to(&server, "fh-sse-s3-stream", "fh-sse-s3").await;
    put_record(&firehose, "fh-sse-s3-stream", b"aes").await;
    let object_key = only_key(&s3, "fh-sse-s3").await;
    let head = s3
        .head_object()
        .bucket("fh-sse-s3")
        .key(&object_key)
        .send()
        .await
        .unwrap();
    assert_eq!(
        head.server_side_encryption(),
        Some(&ServerSideEncryption::Aes256)
    );
    assert_eq!(head.bucket_key_enabled(), Some(true));
}

/// Two rules selecting one object for the same destination replicate it once,
/// by the higher-priority rule; PutBucketReplication refuses rules sharing a
/// priority.
#[tokio::test]
async fn overlapping_rules_replicate_once_and_priorities_must_be_unique() {
    let server = TestServer::start().await;
    let s3 = server.s3_client().await;
    versioned_bucket(&s3, "prio-src").await;
    versioned_bucket(&s3, "prio-dst").await;

    let err = put_replication(
        &s3,
        "prio-src",
        vec![
            rule("a", 1, "star", "prio-dst", false),
            rule("b", 1, "starship", "prio-dst", false),
        ],
    )
    .await
    .expect_err("duplicate priorities are refused");
    assert_eq!(err, "InvalidRequest");

    put_replication(
        &s3,
        "prio-src",
        vec![
            rule("low", 1, "star", "prio-dst", false),
            rule("high", 2, "starship", "prio-dst", false),
        ],
    )
    .await
    .unwrap();
    s3.put_object()
        .bucket("prio-src")
        .key("starship-x")
        .body(ByteStream::from_static(b"one write"))
        .send()
        .await
        .unwrap();
    let versions = s3
        .list_object_versions()
        .bucket("prio-dst")
        .send()
        .await
        .unwrap();
    assert_eq!(versions.versions().len(), 1, "{:?}", versions.versions());
}

/// Versioning cannot be suspended on a replication source, and a delete
/// marker is replicated only by a rule that enables delete marker
/// replication.
#[tokio::test]
async fn replication_source_versioning_and_delete_markers() {
    let server = TestServer::start().await;
    let s3 = server.s3_client().await;
    versioned_bucket(&s3, "dm-src").await;
    versioned_bucket(&s3, "dm-dst").await;
    put_replication(
        &s3,
        "dm-src",
        vec![
            rule("markers", 2, "logs/", "dm-dst", true),
            rule("objects", 1, "", "dm-dst", false),
        ],
    )
    .await
    .unwrap();

    let err = s3
        .put_bucket_versioning()
        .bucket("dm-src")
        .versioning_configuration(
            VersioningConfiguration::builder()
                .status(BucketVersioningStatus::Suspended)
                .build(),
        )
        .send()
        .await
        .expect_err("a replication source cannot suspend versioning");
    assert_eq!(
        err.as_service_error().and_then(|e| {
            use aws_sdk_s3::error::ProvideErrorMetadata;
            e.code()
        }),
        Some("InvalidBucketState")
    );

    for key in ["logs/a", "data/b"] {
        s3.put_object()
            .bucket("dm-src")
            .key(key)
            .body(ByteStream::from_static(b"x"))
            .send()
            .await
            .unwrap();
        s3.delete_object()
            .bucket("dm-src")
            .key(key)
            .send()
            .await
            .unwrap();
    }
    let listing = s3
        .list_object_versions()
        .bucket("dm-dst")
        .send()
        .await
        .unwrap();
    let markers: Vec<&str> = listing
        .delete_markers()
        .iter()
        .filter_map(|m| m.key())
        .collect();
    assert_eq!(markers, vec!["logs/a"]);
    assert_eq!(listing.versions().len(), 2);
}

/// A delivered object in a bucket whose default is DSSE-KMS
/// (`aws:kms:dsse`) records that encryption and reads back as delivered,
/// like a PutObject into the same bucket.
#[tokio::test]
async fn delivered_object_in_a_dsse_kms_bucket_is_readable() {
    let server = TestServer::start().await;
    let s3 = server.s3_client().await;
    let kms = server.kms_client().await;
    let key = kms.create_key().send().await.unwrap();
    let key_arn = key.key_metadata().unwrap().arn().unwrap().to_string();
    s3.create_bucket().bucket("fh-dsse").send().await.unwrap();
    s3.put_bucket_encryption()
        .bucket("fh-dsse")
        .server_side_encryption_configuration(
            ServerSideEncryptionConfiguration::builder()
                .rules(
                    ServerSideEncryptionRule::builder()
                        .apply_server_side_encryption_by_default(
                            ServerSideEncryptionByDefault::builder()
                                .sse_algorithm(ServerSideEncryption::AwsKmsDsse)
                                .kms_master_key_id(&key_arn)
                                .build()
                                .unwrap(),
                        )
                        .build(),
                )
                .build()
                .unwrap(),
        )
        .send()
        .await
        .unwrap();
    let firehose = firehose_to(&server, "fh-dsse-stream", "fh-dsse").await;
    put_record(&firehose, "fh-dsse-stream", b"dual-layer record").await;
    let object_key = only_key(&s3, "fh-dsse").await;
    let got = s3
        .get_object()
        .bucket("fh-dsse")
        .key(&object_key)
        .send()
        .await
        .unwrap();
    assert_eq!(
        got.server_side_encryption(),
        Some(&ServerSideEncryption::AwsKmsDsse)
    );
    let body = got.body.collect().await.unwrap().into_bytes();
    assert_eq!(body.as_ref(), b"dual-layer record");
}
