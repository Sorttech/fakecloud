use aws_sdk_firehose::primitives::Blob;
use aws_sdk_firehose::types::{
    BufferingHints, CompressionFormat, ExtendedS3DestinationConfiguration,
    ExtendedS3DestinationUpdate, ProcessingConfiguration, Processor, ProcessorType, Record,
};
use aws_sdk_s3::types::ChecksumMode;
use fakecloud_testkit::TestServer;
use md5::{Digest, Md5};

#[path = "../../fakecloud-firehose/tests/fixtures/xerial_decoder.rs"]
mod xerial;

async fn snappy_object(
    s3: &aws_sdk_s3::Client,
    bucket: &str,
    extension: &str,
) -> (String, Vec<u8>, Vec<u8>) {
    // Even a zero-interval batch is committed asynchronously by the worker.
    let listing = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let listing = s3.list_objects_v2().bucket(bucket).send().await.unwrap();
            if !listing.contents().is_empty() {
                return listing;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("Firehose Snappy object within 10s");
    assert_eq!(listing.contents().len(), 1);
    assert_eq!(
        listing.contents()[0]
            .storage_class()
            .map(|value| value.as_str()),
        Some("STANDARD")
    );
    let key = listing.contents()[0].key().unwrap().to_owned();
    let listed_etag = listing.contents()[0].e_tag().map(str::to_owned);
    assert!(
        key.ends_with(extension),
        "key {key} must end with {extension}"
    );
    let object = s3
        .get_object()
        .bucket(bucket)
        .key(&key)
        .checksum_mode(ChecksumMode::Enabled)
        .send()
        .await
        .unwrap();
    assert_eq!(object.content_type(), Some("application/octet-stream"));
    assert_eq!(object.content_encoding(), Some("snappy-java"));
    let checksum = object
        .checksum_crc64_nvme()
        .expect("default CRC64NVME checksum")
        .to_owned();
    assert_eq!(
        object.checksum_type().map(|value| value.as_str()),
        Some("FULL_OBJECT")
    );
    let etag = object.e_tag().map(str::to_owned);
    let content_length = object.content_length().unwrap();
    let raw = object.body.collect().await.unwrap().into_bytes().to_vec();
    assert_eq!(content_length, raw.len() as i64);
    // The ETag is the MD5 of the stored (Snappy-encoded) bytes, as for GZIP.
    let md5_etag = format!("\"{:x}\"", Md5::digest(&raw));
    assert_eq!(etag.as_deref(), Some(md5_etag.as_str()));
    assert_eq!(listed_etag.as_deref(), Some(md5_etag.as_str()));
    let decoded = xerial::decode(&raw).unwrap().concat();
    let head = s3
        .head_object()
        .bucket(bucket)
        .key(&key)
        .checksum_mode(ChecksumMode::Enabled)
        .send()
        .await
        .unwrap();
    assert_eq!(head.e_tag(), Some(md5_etag.as_str()));
    assert_eq!(head.checksum_crc64_nvme(), Some(checksum.as_str()));
    assert_eq!(
        head.checksum_type().map(|value| value.as_str()),
        Some("FULL_OBJECT")
    );
    (key, raw, decoded)
}

#[tokio::test]
async fn firehose_snappy_binary_multiblock_and_extension_overrides() {
    // Validate this test's separate decoder against Java-produced streams before
    // using it to judge output fetched through the official AWS SDK.
    assert_eq!(
        xerial::decode(include_bytes!(
            "../../fakecloud-firehose/tests/fixtures/xerial_binary.snappy"
        ))
        .unwrap()
        .concat(),
        xerial::binary_input()
    );
    assert_eq!(
        xerial::decode(include_bytes!(
            "../../fakecloud-firehose/tests/fixtures/xerial_multiblock.snappy"
        ))
        .unwrap()
        .concat(),
        xerial::multiblock_input()
    );

    let server = TestServer::start().await;
    let s3 = server.s3_client().await;
    let firehose = server.firehose_client().await;
    for (i, (extension, expected_extension, input)) in [
        (None, ".snappy", xerial::binary_input()),
        (Some(""), ".snappy", xerial::multiblock_input()),
        (
            Some(".custom.log"),
            ".custom.log",
            xerial::incompressible_input(70000),
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let bucket = format!("fh-snappy-{i}");
        let stream = format!("snappy-{i}");
        s3.create_bucket().bucket(&bucket).send().await.unwrap();
        firehose
            .create_delivery_stream()
            .delivery_stream_name(&stream)
            .extended_s3_destination_configuration(
                ExtendedS3DestinationConfiguration::builder()
                    .role_arn("arn:aws:iam::123456789012:role/firehose")
                    .bucket_arn(format!("arn:aws:s3:::{bucket}"))
                    .buffering_hints(
                        BufferingHints::builder()
                            .size_in_mbs(5)
                            .interval_in_seconds(0)
                            .build(),
                    )
                    .compression_format(CompressionFormat::Snappy)
                    .set_file_extension(extension.map(str::to_owned))
                    .build()
                    .unwrap(),
            )
            .send()
            .await
            .unwrap();
        let split = if input.len() > 32768 { 32760 } else { 17 };
        let response = firehose
            .put_record_batch()
            .delivery_stream_name(&stream)
            .records(
                Record::builder()
                    .data(Blob::new(&input[..split]))
                    .build()
                    .unwrap(),
            )
            .records(
                Record::builder()
                    .data(Blob::new(&input[split..]))
                    .build()
                    .unwrap(),
            )
            .send()
            .await
            .unwrap();
        assert_eq!(response.failed_put_count(), 0);
        assert_eq!(response.request_responses().len(), 2);
        let (_, raw, decoded) = snappy_object(&s3, &bucket, expected_extension).await;
        assert_eq!(decoded, input);
        let blocks = xerial::decode(&raw).unwrap();
        let expected_sizes: Vec<usize> = input.chunks(32768).map(<[u8]>::len).collect();
        assert_eq!(
            blocks.iter().map(Vec::len).collect::<Vec<_>>(),
            expected_sizes
        );
        if i == 2 {
            assert!(
                raw.len() > input.len(),
                "incompressible blocks can expand without losing data"
            );
        }
    }
}

/// `AppendDelimiterToRecord` adds `\n` after every record before compression.
/// The delimiter is ordinary payload in Snappy's 32 KiB input blocks, so it
/// can close a block instead of starting a new one.
#[tokio::test]
async fn firehose_snappy_delimiters_share_block_windows() {
    let server = TestServer::start().await;
    let s3 = server.s3_client().await;
    let firehose = server.firehose_client().await;
    let bucket = "fh-snappy-delimited";
    s3.create_bucket().bucket(bucket).send().await.unwrap();
    firehose
        .create_delivery_stream()
        .delivery_stream_name("snappy-delimited")
        .extended_s3_destination_configuration(
            ExtendedS3DestinationConfiguration::builder()
                .role_arn("arn:aws:iam::123456789012:role/firehose")
                .bucket_arn(format!("arn:aws:s3:::{bucket}"))
                // Zero interval: the batch is due at admission; the helper
                // polls for the asynchronous write.
                .buffering_hints(
                    BufferingHints::builder()
                        .size_in_mbs(5)
                        .interval_in_seconds(0)
                        .build(),
                )
                .compression_format(CompressionFormat::Snappy)
                .processing_configuration(
                    ProcessingConfiguration::builder()
                        .enabled(true)
                        .processors(
                            Processor::builder()
                                .r#type(ProcessorType::AppendDelimiterToRecord)
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
    let response = firehose
        .put_record_batch()
        .delivery_stream_name("snappy-delimited")
        .records(
            Record::builder()
                .data(Blob::new(vec![b'x'; 32767]))
                .build()
                .unwrap(),
        )
        .records(Record::builder().data(Blob::new(b"y")).build().unwrap())
        .records(Record::builder().data(Blob::new([0, 255])).build().unwrap())
        .send()
        .await
        .unwrap();
    assert_eq!(response.failed_put_count(), 0);
    let (_, raw, decoded) = snappy_object(&s3, bucket, ".snappy").await;
    let mut expected = vec![b'x'; 32767];
    expected.extend_from_slice(b"\ny\n\x00\xff\n");
    assert_eq!(decoded, expected);
    let blocks = xerial::decode(&raw).unwrap();
    assert_eq!(blocks.iter().map(Vec::len).collect::<Vec<_>>(), [32768, 5]);
    assert_eq!(blocks[0][32767], b'\n');
    assert_eq!(blocks[1], b"y\n\x00\xff\n");
}

/// Delivery reads the stream's current destination, so switching an
/// uncompressed stream to `Snappy` changes the next object's encoding.
#[tokio::test]
async fn firehose_update_destination_switches_delivery_to_snappy() {
    let server = TestServer::start().await;
    let s3 = server.s3_client().await;
    let firehose = server.firehose_client().await;
    let bucket = "fh-snappy-updated";
    s3.create_bucket().bucket(bucket).send().await.unwrap();
    firehose
        .create_delivery_stream()
        .delivery_stream_name("snappy-updated")
        .extended_s3_destination_configuration(
            ExtendedS3DestinationConfiguration::builder()
                .role_arn("arn:aws:iam::123456789012:role/firehose")
                .bucket_arn(format!("arn:aws:s3:::{bucket}"))
                // Zero interval: the record is due at admission; the helper
                // polls for the asynchronous write.
                .buffering_hints(
                    BufferingHints::builder()
                        .size_in_mbs(5)
                        .interval_in_seconds(0)
                        .build(),
                )
                .compression_format(CompressionFormat::Uncompressed)
                .build()
                .unwrap(),
        )
        .send()
        .await
        .unwrap();
    let described = firehose
        .describe_delivery_stream()
        .delivery_stream_name("snappy-updated")
        .send()
        .await
        .unwrap();
    let description = described.delivery_stream_description().unwrap();
    firehose
        .update_destination()
        .delivery_stream_name("snappy-updated")
        .current_delivery_stream_version_id(description.version_id())
        .destination_id(description.destinations()[0].destination_id())
        .extended_s3_destination_update(
            ExtendedS3DestinationUpdate::builder()
                .compression_format(CompressionFormat::Snappy)
                .build(),
        )
        .send()
        .await
        .unwrap();
    firehose
        .put_record()
        .delivery_stream_name("snappy-updated")
        .record(
            Record::builder()
                .data(Blob::new(b"after update"))
                .build()
                .unwrap(),
        )
        .send()
        .await
        .unwrap();
    let (_, _, decoded) = snappy_object(&s3, bucket, ".snappy").await;
    assert_eq!(decoded, b"after update");
}

#[tokio::test]
async fn firehose_snappy_object_and_configuration_survive_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let mut server = TestServer::start_persistent(tmp.path()).await;
    let s3 = server.s3_client().await;
    let firehose = server.firehose_client().await;
    let bucket = "fh-snappy-persistent";
    s3.create_bucket().bucket(bucket).send().await.unwrap();
    firehose
        .create_delivery_stream()
        .delivery_stream_name("snappy-durable")
        .extended_s3_destination_configuration(
            ExtendedS3DestinationConfiguration::builder()
                .role_arn("arn:aws:iam::123456789012:role/firehose")
                .bucket_arn(format!("arn:aws:s3:::{bucket}"))
                .buffering_hints(
                    BufferingHints::builder()
                        .size_in_mbs(5)
                        .interval_in_seconds(0)
                        .build(),
                )
                .prefix("events/")
                .compression_format(CompressionFormat::Snappy)
                .build()
                .unwrap(),
        )
        .send()
        .await
        .unwrap();
    let input = xerial::incompressible_input(70000);
    firehose
        .put_record()
        .delivery_stream_name("snappy-durable")
        .record(
            Record::builder()
                .data(Blob::new(input.clone()))
                .build()
                .unwrap(),
        )
        .send()
        .await
        .unwrap();
    let before = snappy_object(&s3, bucket, ".snappy").await;
    assert_eq!(before.2, input);
    server.restart().await;
    let s3 = server.s3_client().await;
    assert_eq!(snappy_object(&s3, bucket, ".snappy").await, before);
    let firehose = server.firehose_client().await;
    let response = firehose
        .describe_delivery_stream()
        .delivery_stream_name("snappy-durable")
        .send()
        .await
        .unwrap();
    let destination = response
        .delivery_stream_description()
        .unwrap()
        .destinations()[0]
        .extended_s3_destination_description()
        .unwrap();
    assert_eq!(destination.compression_format(), &CompressionFormat::Snappy);
}
