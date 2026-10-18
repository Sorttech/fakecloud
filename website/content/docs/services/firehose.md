+++
title = "Firehose"
description = "Amazon Data Firehose delivery streams and buffered S3 delivery with uncompressed, GZIP, or Xerial Snappy output, JSON 1.1 protocol."
weight = 32
+++

fakecloud implements Amazon Data Firehose's JSON 1.1 control plane and buffered delivery to [S3](/docs/services/s3/) with uncompressed, GZIP, or Xerial Snappy output. Valid records from separate `PutRecord` and `PutRecordBatch` calls share a stream buffer. A background worker delivers objects when the decoded-byte size threshold or the oldest record's interval is reached, including when ingestion has gone idle.

**Status: control-plane parity and buffered S3 delivery. Unsupported destination and pipeline configurations are stored, but record admission rejects them rather than acknowledging delivery that cannot run.**

## Supported today

- **Delivery streams**: `CreateDeliveryStream` / `DescribeDeliveryStream` / `ListDeliveryStreams` / `DeleteDeliveryStream` / `UpdateDestination`. Both `DirectPut` and `KinesisStreamAsSource` source types round-trip; Kinesis-source polling is not implemented. Created streams are immediately `ACTIVE` and deletion removes them immediately.
- **Destinations**: `ExtendedS3DestinationConfiguration`, `RedshiftDestinationConfiguration`, `ElasticsearchDestinationConfiguration`, `AmazonopensearchserviceDestinationConfiguration`, `SplunkDestinationConfiguration`, `HttpEndpointDestinationConfiguration`, `SnowflakeDestinationConfiguration`, `IcebergDestinationConfiguration`. Configuration round-trips verbatim through `UpdateDestination`.
- **Buffering hints**: omitted hints default to **5 MiB / 300 seconds**. If `BufferingHints` on `CreateDeliveryStream` or `UpdateDestination` supplies either member, it must supply both. An empty `BufferingHints` object is accepted: it uses the defaults on create and keeps the current hints on update.
  - `SizeInMBs`: 1-128, each unit is 1,048,576 decoded bytes.
  - `IntervalInSeconds`: 0-900, including short intervals such as 2 seconds.

  Incomplete pairs, non-integers, and out-of-range values return `InvalidArgumentException`. CloudFormation also accepts quoted integers such as `"60"`, because `Ref` parameters resolve to strings; it coerces them, and quoted `Enabled` flags, including inside `S3BackupConfiguration`, before validating. Interval zero makes nonempty input eligible at admission; writing is still asynchronous, so poll S3 for completion rather than assuming an acknowledgment means the object exists.
- **Destination strings**: the AWS model's length limits apply: `Prefix` and `ErrorOutputPrefix` allow at most 1,024 characters, `CustomTimeZone` at most 50, `RoleARN` 1-512, and `BucketARN` 1-2,048. ARN and time-zone patterns are not enforced here. Other values return `InvalidArgumentException` from `CreateDeliveryStream` and `UpdateDestination`, and make a CloudFormation resource fail.
- **Records**: `PutRecord` / `PutRecordBatch` assign per-record `RecordId`s after supported S3 admission. Binary bytes and producer-supplied delimiters are preserved exactly before compression; no separators are added unless the `AppendDelimiterToRecord` processor is enabled. Without it, a delivery window holding only empty records produces no object. Public API and internal Logs/SES delivery use the same buffering engine.
  Each API record is limited to 1,024,000 decoded bytes; a batch contains 1-500 records and at most 4,194,304 decoded bytes. Request-wide limit violations reject the whole call before admission. Invalid-base64 batch entries receive individual errors, while valid entries can still be admitted.
- **New line delimiter**: when `ProcessingConfiguration` has `Enabled: true` and every entry in its `Processors` is `AppendDelimiterToRecord`, a newline (`\n`) is written after every record, including the last one and empty records, before any compression; a delivery window holding only empty records becomes an object of newlines. This applies to API deliveries, to internal CloudWatch Logs and SES deliveries, and to streams created or updated through CloudFormation, which store `ProcessingConfiguration` like `CreateDeliveryStream`. The optional `Delimiter` parameter is stored but not interpreted: AWS documents only a new line delimiter. Appended newlines do not count toward the size threshold. Lambda and other processors are not run; enabled processing that includes them, or lists no processors, rejects record admission with HTTP 400 `InvalidArgumentException` naming the unsupported setting (see [Caveats](#caveats)).
- **S3 output**: static prefixes, UTC date hierarchy, stream/version/time/UUID object names, and `FileExtension` overrides. Objects have `Content-Type: application/octet-stream` and the encoded body's MD5 as their `ETag`. GZIP output has `Content-Encoding: gzip`; Snappy output has `Content-Encoding: snappy-java`; uncompressed output has no content encoding. Completed objects' bytes and metadata survive persistent-mode restart, including internal adapter delivery. Bucket versioning, default Object Lock retention, bucket-default encryption (including SSE-KMS), replication, and S3 event notifications apply through the shared S3 write path as for ordinary S3 writes, including for compressed objects. `EncryptionConfiguration.KMSEncryptionConfig` writes objects with SSE-KMS under the configured key (see [Caveats](#caveats)). Pending buffers and retry queues are process-local and are not persisted.
- **Tags**: `ListTagsForDeliveryStream` / `TagDeliveryStream` / `UntagDeliveryStream`. Keyed by stream ARN.
- **Server-side encryption**: `StartDeliveryStreamEncryption` / `StopDeliveryStreamEncryption`. Start sets the stream's `DeliveryStreamEncryptionConfiguration` to `Status=ENABLED` with the requested `KeyType` (defaults `AWS_OWNED_CMK`; stores the `KeyARN` for `CUSTOMER_MANAGED_CMK`); Stop flips it to `DISABLED`. The config is surfaced on `DescribeDeliveryStream`. Unknown streams return `ResourceNotFoundException`.

## Buffering and destination updates

Size accounting uses decoded record bytes, before compression or base64 transport encoding; newlines added by `AppendDelimiterToRecord` are not counted. Delivery becomes eligible at **greater than or equal to** the configured threshold. All valid records in one API call are admitted together: a batch crossing the threshold stays whole, so the resulting object can overshoot the hint. Highly compressible input can trigger delivery even when the encoded object is much smaller than the threshold.

The interval starts with the oldest arrival in a new buffer. After that buffer is detached for delivery, subsequent records start a fresh size/time window. Buffers are isolated by account, region, and delivery-stream instance.

`UpdateDestination` affects data still in the active buffer: when that data becomes eligible, it uses the latest supported destination, hints, output settings, and stream version. Once a batch has detached, its destination, version, codec, destination encryption key, timestamps, and object key are captured and reused for every retry. An unsupported update pauses already accepted active data until the configuration becomes supported again; it does not discard that data. Detached batches continue with their captured configuration. Deleting a stream or resetting the server discards its pending data; recreating the same stream name does not recover the old buffer.

## S3 object bytes and names

Create the destination bucket before sending records. Records `abc`, `def\n`, and the binary bytes `00 ff` in one delivery window become exactly `abcdef\n` followed by `00 ff`, whether submitted in one batch or separate calls. For compressed output, those concatenated bytes are encoded in one GZIP or Xerial Snappy stream. Producers must supply their own delimiters, such as a newline for each JSON record, or enable `AppendDelimiterToRecord`, which writes the same records as exactly `abc\ndef\n\n` followed by `00 ff 0a` before any compression.

CloudWatch Logs subscription records arrive as GZIP-compressed bytes, as on AWS: each delivery is one record holding one GZIP-compressed `DATA_MESSAGE` envelope, with no base64 layer. An object that buffers several deliveries is their GZIP members concatenated with no separator; a multi-member GZIP reader such as `gunzip` or `zcat` returns the JSON envelopes back to back. Choosing `GZIP` compression on the stream compresses those members again, as on AWS, which documents that this data needs no Firehose compression; decompress such an object once to recover the members.

For a static prefix, the key format is:

```text
<prefix>yyyy/MM/dd/HH/<stream>-<version>-yyyy-MM-dd-HH-mm-ss-<uuid><extension>
```

The prefix's date hierarchy uses the **oldest record's arrival time in UTC**. The suffix timestamp uses the time the batch is detached for delivery, also UTC. Retries keep that captured key rather than creating new timestamps or UUIDs. The version comes from the configuration at detachment and increments on `UpdateDestination`. The UUID is a full hyphenated UUID.

An absent or empty prefix starts at `yyyy/MM/dd/HH/`. A prefix of `events/` produces `events/yyyy/MM/dd/HH/`; `events` produces `eventsyyyy/MM/dd/HH/`, without an inserted slash.

| `CompressionFormat` | Default extension | `Content-Encoding` |
| --- | --- | --- |
| `UNCOMPRESSED` (also the default when omitted) | None | None |
| `GZIP` | `.gz` | `gzip` |
| `Snappy` | `.snappy` | `snappy-java` |

Uncompressed output has no default extension. `FileExtension: ".custom.log"` replaces the default extension with `.custom.log`; an absent or empty override keeps the codec's default. Naming an object `.log` does not disable compression. `FileExtension` must be empty or a dot followed by lowercase letters, digits, or `!-_.*'()`, with at most 128 characters; other values return `InvalidArgumentException` from `CreateDeliveryStream` and `UpdateDestination`, and make a CloudFormation `AWS::KinesisFirehose::DeliveryStream` resource fail. `Content-Length` is the encoded object's byte length. S3 reads return the stored compressed bytes; decompress them to recover the original record bytes.

Use the exact enum spelling `Snappy` in `CompressionFormat`. Its output uses the Xerial/snappy-java container: a 16-byte version header followed by length-prefixed raw Snappy blocks, each covering up to 32 KiB of input. Record boundaries do not end a block, and each block is compressed independently. Read it with an Xerial-aware decoder; this is distinct from the standard Snappy framed-stream format and Hadoop-Snappy.

## Smoke test

```sh
fakecloud &

aws --endpoint-url http://localhost:4566 s3api create-bucket --bucket my-bucket

aws --endpoint-url http://localhost:4566 firehose create-delivery-stream \
  --delivery-stream-name events \
  --delivery-stream-type DirectPut \
  --extended-s3-destination-configuration '{
    "RoleARN": "arn:aws:iam::000000000000:role/firehose",
    "BucketARN": "arn:aws:s3:::my-bucket",
    "BufferingHints": {"SizeInMBs": 5, "IntervalInSeconds": 0},
    "CompressionFormat": "UNCOMPRESSED",
    "Prefix": "events/",
    "FileExtension": ".json"
  }'

aws --endpoint-url http://localhost:4566 firehose describe-delivery-stream \
  --delivery-stream-name events

aws --endpoint-url http://localhost:4566 firehose put-record \
  --delivery-stream-name events \
  --record Data=$(echo -n '{"id":"abc"}' | base64)

# Zero interval makes delivery eligible immediately, but the writer is async.
# Poll at most 20 times, sleeping 0.5 seconds after each empty result.
# CLI request latency and retries add time; this is not a wall-clock timeout.
# jq is used to inspect the object count.
for attempt in $(seq 1 20); do
  objects=$(aws --endpoint-url http://localhost:4566 s3api list-objects-v2 --bucket my-bucket)
  [ "$(printf '%s' "$objects" | jq '.Contents | length')" -gt 0 ] && break
  sleep 0.5
done
printf '%s\n' "$objects"
# Read the returned key with s3api get-object to inspect the original bytes.
```

## Introspection

`GET /_fakecloud/firehose/delivery-streams` is an IAM-bypass admin endpoint that returns every delivery stream across all accounts and regions, so tests can assert stream state without round-tripping through `DescribeDeliveryStream` (and without credentials):

```sh
curl -fsS http://localhost:4566/_fakecloud/firehose/delivery-streams | jq
```

Response shape:

```json
{
  "deliveryStreams": [
    {
      "accountId": "123456789012",
      "name": "fh-intro",
      "arn": "arn:aws:firehose:us-east-1:123456789012:deliverystream/fh-intro",
      "streamType": "DirectPut",
      "status": "ACTIVE",
      "encryption": { "status": "ENABLED", "keyType": "AWS_OWNED_CMK" },
      "destinationCount": 1,
      "createTimestamp": "2026-05-29T00:00:00+00:00",
      "lastUpdateTimestamp": "2026-05-29T00:00:00+00:00"
    }
  ]
}
```

`encryption` is derived from the persisted SSE config: `{ "status": "DISABLED" }` when no encryption is configured, or `{ "status": "ENABLED", "keyType": ..., "keyArn": ... }` after `StartDeliveryStreamEncryption` (the `keyArn` is present only for customer-managed keys). Streams are sorted by account, then name.

All first-party SDKs ship a `firehose` sub-client wrapping this endpoint (`getDeliveryStreams()`):

- Rust: `fakecloud_sdk::FakeCloud::new(url).firehose().get_delivery_streams()`
- Go: `fakecloud.New(url).Firehose().GetDeliveryStreams(ctx)`
- Python: `await fc.firehose.get_delivery_streams()` (async) or `fc.firehose.get_delivery_streams()` (sync)
- TypeScript: `await fc.firehose.getDeliveryStreams()`
- Java: `fc.firehose().getDeliveryStreams()`
- PHP: `$fc->firehose()->getDeliveryStreams()`

See [`reference/introspection`](/docs/reference/introspection/) for the full endpoint catalog.

## Caveats

Only S3 delivery with `UNCOMPRESSED`, `GZIP`, or `Snappy`, static prefixes, and UTC is implemented. Unsupported codecs (`ZIP` and `HADOOP_SNAPPY`), prefix expressions (`!{...}`), non-UTC time zones, enabled processing other than `AppendDelimiterToRecord` alone (a Lambda or other processor, even alongside `AppendDelimiterToRecord`, or no processors), enabled format conversion (including an omitted `Enabled` member), enabled dynamic partitioning, S3 backup pipelines, and non-S3 destinations return **HTTP 400 `InvalidArgumentException`** with a message naming the unsupported setting before `PutRecord` or `PutRecordBatch` acknowledges records. All modeled codec values and these destination configurations still round-trip through `CreateDeliveryStream`, `UpdateDestination`, and CloudFormation; they do not silently deliver unprocessed bytes. Disabled backup, `NoEncryption`, and other disabled capabilities remain usable. `ErrorOutputPrefix` and `CloudWatchLoggingOptions` are accepted and stored but do not change delivery.

Internal CloudWatch Logs subscription and SES event deliveries have no caller to receive an error. A record for a missing stream, for a stream with one of the unsupported settings above, or arriving after shutdown has closed admission is dropped and logged as a warning.

S3 backup pipelines and non-S3 destinations are accepted as configuration but rejected at record admission as described above. The destination bucket's default encryption (including SSE-KMS), replication, and S3 event notifications really apply to encoded objects through the shared S3 write path.

A destination's `EncryptionConfiguration.KMSEncryptionConfig` writes every delivered object with SSE-KMS under `AWSKMSKeyARN`, overriding the bucket's default encryption: `HeadObject`/`GetObject` report `ServerSideEncryption: aws:kms` and that key ARN, and reads return the decrypted (still compressed, for GZIP or Snappy output) bytes. With `NoEncryptionConfig` or no `EncryptionConfiguration`, the bucket default applies. The key is captured when a batch detaches, so a later `UpdateDestination` does not change the key of a batch already awaiting delivery or retry. A key that cannot encrypt (unknown, disabled, or not usable by the bucket's account) is a delivery failure: the records were already acknowledged, and the batch is retained and retried as described under [Retries, persistence, and shutdown](#retries-persistence-and-shutdown). Firehose stream encryption configured by `StartDeliveryStreamEncryption` is separate from destination-requested object encryption. `KinesisStreamAsSource` remains a stored source configuration; there is no Kinesis-source polling.

Delivery does not assume `RoleARN` or evaluate IAM policies, bucket policies, or `--iam strict` for Firehose: the stream writes into the configured bucket of its own account as that bucket's owner would. CloudWatch Logs subscription filters and SES event destinations may name a delivery stream in another account, and their records are delivered into that account's stream. A delivery stream stored without an S3 destination (for example, created with an S3 configuration missing `RoleARN` or `BucketARN`, which AWS rejects) acknowledges records without writing them.

## Retries, persistence, and shutdown

An acknowledgment means accepted into process-local memory, not durably delivered. A malformed S3 bucket ARN is accepted by `CreateDeliveryStream`, `UpdateDestination`, and CloudFormation, but record admission rejects it with `InvalidArgumentException`. A well-formed ARN naming a missing bucket is accepted; failed delivery is retained and retried, so creating that bucket later unblocks delivery at the next retry. Encoding and storage failures also retain the batch. Each stream has a FIFO delivery queue and at most one in-flight write; a failed head blocks later batches in that stream, while other streams can continue. At most four S3 writes run at once across all streams, scheduled in rotation so no stream starves.

Runtime retries reuse the same captured key and output configuration. As AWS documents for S3 destinations, a failed head is retried every 5 seconds, with no backoff. Retries start on the worker's 1-second tick, so each actual delay can run up to one second longer. Each failed retry logs a warning with the attempt number and the next retry delay. As on AWS for `DirectPut` streams, retries stop 24 hours after a batch's oldest record arrived: the next failed attempt after that discards the batch with a warning naming its record and byte counts, and later batches in the stream proceed. The shutdown drain still makes its single final attempt regardless of age.

**Memory policy:** failed batches remain in memory for up to 24 hours while new valid records continue to be accepted. There is no memory limit, backpressure, or durable pending spool, so memory use can grow with everything accepted during that window of prolonged failure; a process crash loses pending data. Persistent mode stores stream configuration and completed S3 objects, not buffers or retry queues.

**Shutdown policy:** graceful shutdown first lets the HTTP server finish in-flight requests, so API callers never see the closed admission. It then closes admission, waits for in-flight writes, and makes one final drain of supported buffered data without waiting for normal size/time hints or the retry interval. If a final head fails, later batches in that stream are not attempted; shutdown reports undelivered record/decoded-byte counts and does not start another retry loop. Unsupported active data is also reported undelivered. Late internal handoffs are refused with `ServiceUnavailableException` (HTTP 503) and dropped. The final drain has no time limit: a large backlog or slow store prolongs shutdown, and a supervisor that kills the process first (for example, after Docker's default 10-second stop timeout) loses whatever was not yet written. With a healthy bucket and store, graceful restart drains buffered records into durable objects; failed final-drain data is not recovered after restart. No flush/admin endpoint is provided.

## Source

- [`crates/fakecloud-firehose`](https://github.com/faiscadev/fakecloud/tree/main/crates/fakecloud-firehose)
- [Amazon Data Firehose API reference](https://docs.aws.amazon.com/firehose/latest/APIReference/Welcome.html)
