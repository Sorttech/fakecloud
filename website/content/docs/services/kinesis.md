+++
title = "Kinesis"
description = "Data Streams, records, shard iterators, retention, tagging."
weight = 16
+++

fakecloud implements **44 of 44** Kinesis operations at 100% Smithy conformance.

## Supported features

- **Streams** — CreateStream, DeleteStream, ListStreams, DescribeStream, DescribeStreamSummary
- **Records** — PutRecord, PutRecords, GetRecords, GetShardIterator
- **Shard management** — SplitShard, MergeShards, ListShards, DescribeStreamConsumer
- **Retention** — IncreaseStreamRetentionPeriod, DecreaseStreamRetentionPeriod
- **Tags** — AddTagsToStream, RemoveTagsFromStream, ListTagsForStream
- **Stream modes** — ON_DEMAND and PROVISIONED
- **Encryption** — StartStreamEncryption, StopStreamEncryption
- **Consumers** — EnableEnhancedMonitoring, DisableEnhancedMonitoring
- **Cross-stream** — MergeShards, SplitShard
- **Resource policies** — PutResourcePolicy, GetResourcePolicy, DeleteResourcePolicy
- **Delivery channels** — CreateChannel, DescribeChannel, ListChannels, UpdateChannel, DeleteChannel: a channel fans records from one or more source streams into an S3 bucket or into Apache Iceberg tables on S3 Tables, with the destination, freshness, encryption, and CloudWatch logging configuration persisted and round-tripped. A stream cannot be deleted while a channel still draws from it.

## Protocol

JSON protocol. `X-Amz-Target` header, JSON body, JSON responses.

## Cross-service delivery

- **Kinesis -> Lambda** — Event source mapping polls shards and invokes functions
- **DynamoDB -> Kinesis** — Table changes stream to Kinesis Data Streams

## Source

- [`crates/fakecloud-kinesis`](https://github.com/faiscadev/fakecloud/tree/main/crates/fakecloud-kinesis)
- [AWS Kinesis API reference](https://docs.aws.amazon.com/kinesis/latest/APIReference/Welcome.html)
