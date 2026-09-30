//! KMS aliases are regional resources: the same alias name exists
//! independently in every region, `ListAliases` pages through the request
//! region's aliases only, an alias name resolves in the caller's region and an
//! alias ARN in the region it names, and `alias/aws/<service>` names each
//! region's own AWS-managed key. Services resolving a customer alias (S3,
//! DynamoDB) resolve it in the resource's region.

mod helpers;

use aws_sdk_kms::primitives::Blob;
use helpers::TestServer;

async fn kms_in(server: &TestServer, region: &str) -> aws_sdk_kms::Client {
    aws_sdk_kms::Client::new(&server.aws_config_in(region).await)
}

async fn create_key(kms: &aws_sdk_kms::Client) -> (String, String) {
    let meta = kms
        .create_key()
        .send()
        .await
        .expect("CreateKey")
        .key_metadata
        .expect("key metadata");
    (meta.key_id().to_string(), meta.arn().unwrap().to_string())
}

async fn describe_arn(kms: &aws_sdk_kms::Client, key: &str) -> Option<String> {
    kms.describe_key()
        .key_id(key)
        .send()
        .await
        .ok()
        .and_then(|r| r.key_metadata)
        .and_then(|m| m.arn)
}

/// Every alias of `kms`'s region, paging `limit` at a time, as (name, ARN,
/// target key id).
async fn list_all_aliases(
    kms: &aws_sdk_kms::Client,
    limit: i32,
) -> Vec<(String, String, Option<String>)> {
    let mut out = Vec::new();
    let mut marker: Option<String> = None;
    loop {
        let page = kms
            .list_aliases()
            .limit(limit)
            .set_marker(marker.clone())
            .send()
            .await
            .expect("ListAliases");
        for a in page.aliases() {
            out.push((
                a.alias_name().unwrap().to_string(),
                a.alias_arn().unwrap().to_string(),
                a.target_key_id().map(str::to_string),
            ));
        }
        if !page.truncated() {
            break;
        }
        marker = page.next_marker().map(str::to_string);
        assert!(marker.is_some(), "truncated page without NextMarker");
    }
    out
}

#[tokio::test]
async fn same_alias_name_lives_independently_in_each_region() {
    let server = TestServer::start().await;
    let east = kms_in(&server, "us-east-1").await;
    let west = kms_in(&server, "eu-west-1").await;
    let (east_id, east_arn) = create_key(&east).await;
    let (west_id, west_arn) = create_key(&west).await;

    for (kms, key) in [(&east, &east_id), (&west, &west_id)] {
        kms.create_alias()
            .alias_name("alias/app")
            .target_key_id(key)
            .send()
            .await
            .expect("CreateAlias of the same name in a second region");
    }
    let dup = west
        .create_alias()
        .alias_name("alias/app")
        .target_key_id(&west_id)
        .send()
        .await
        .expect_err("same name twice in one region");
    assert!(
        dup.into_service_error().is_already_exists_exception(),
        "duplicate in-region alias is AlreadyExists"
    );

    // An alias name resolves in the caller's region.
    assert_eq!(
        describe_arn(&east, "alias/app").await,
        Some(east_arn.clone())
    );
    assert_eq!(
        describe_arn(&west, "alias/app").await,
        Some(west_arn.clone())
    );
    let south = kms_in(&server, "ap-south-1").await;
    assert_eq!(describe_arn(&south, "alias/app").await, None);

    // An alias ARN resolves in the region it names, from any region.
    let west_alias_arn = "arn:aws:kms:eu-west-1:123456789012:alias/app";
    assert_eq!(
        describe_arn(&east, west_alias_arn).await,
        Some(west_arn.clone())
    );
    let enc = east
        .encrypt()
        .key_id(west_alias_arn)
        .plaintext(Blob::new(b"cross-region".to_vec()))
        .send()
        .await
        .expect("Encrypt under another region's alias ARN");
    assert_eq!(enc.key_id(), Some(west_arn.as_str()));

    // ListAliases is per region, with the region's alias ARN.
    for (kms, region, key) in [
        (&east, "us-east-1", &east_id),
        (&west, "eu-west-1", &west_id),
    ] {
        let customer: Vec<_> = list_all_aliases(kms, 100)
            .await
            .into_iter()
            .filter(|(name, _, _)| !name.starts_with("alias/aws/"))
            .collect();
        assert_eq!(
            customer,
            vec![(
                "alias/app".to_string(),
                format!("arn:aws:kms:{region}:123456789012:alias/app"),
                Some(key.clone()),
            )],
            "{region}"
        );
    }

    // An alias and its key share a region.
    let err = west
        .create_alias()
        .alias_name("alias/cross")
        .target_key_id(&east_arn)
        .send()
        .await
        .expect_err("alias in eu-west-1 on a us-east-1 key");
    assert!(err.into_service_error().is_not_found_exception());

    // Deleting one region's alias leaves the other region's.
    east.delete_alias()
        .alias_name("alias/app")
        .send()
        .await
        .expect("DeleteAlias");
    assert_eq!(describe_arn(&east, "alias/app").await, None);
    assert_eq!(describe_arn(&west, "alias/app").await, Some(west_arn));
}

#[tokio::test]
async fn list_aliases_pages_through_the_request_region_only() {
    let server = TestServer::start().await;
    let east = kms_in(&server, "us-east-1").await;
    let west = kms_in(&server, "eu-west-1").await;
    let (east_id, _) = create_key(&east).await;
    let (west_id, _) = create_key(&west).await;
    for i in 0..5 {
        east.create_alias()
            .alias_name(format!("alias/east-{i}"))
            .target_key_id(&east_id)
            .send()
            .await
            .unwrap();
    }
    for i in 0..3 {
        west.create_alias()
            .alias_name(format!("alias/west-{i}"))
            .target_key_id(&west_id)
            .send()
            .await
            .unwrap();
    }

    for (kms, own, other) in [
        (&east, "alias/east-", "alias/west-"),
        (&west, "alias/west-", "alias/east-"),
    ] {
        let full = list_all_aliases(kms, 100).await;
        let paged = list_all_aliases(kms, 2).await;
        assert_eq!(
            paged, full,
            "Limit=2 pages cover every alias once, in order"
        );
        let names: Vec<&str> = full.iter().map(|(n, _, _)| n.as_str()).collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted, "AliasName order");
        assert!(names.iter().any(|n| n.starts_with(own)));
        assert!(!names.iter().any(|n| n.starts_with(other)));
        assert!(
            names.contains(&"alias/aws/s3"),
            "managed aliases pre-listed"
        );
    }

    // Each region's alias/aws/s3 names that region's own AWS-managed key.
    let east_s3 = describe_arn(&east, "alias/aws/s3").await.unwrap();
    let west_s3 = describe_arn(&west, "alias/aws/s3").await.unwrap();
    assert!(east_s3.starts_with("arn:aws:kms:us-east-1:"), "{east_s3}");
    assert!(west_s3.starts_with("arn:aws:kms:eu-west-1:"), "{west_s3}");
}

/// A table encrypted with no named key reports its region's AWS-managed
/// `aws/dynamodb` key, which is what `alias/aws/dynamodb` resolves to in that
/// region; a named alias resolves in the table's region.
#[tokio::test]
async fn dynamodb_keys_follow_the_table_region() {
    let server = TestServer::start().await;
    let mut managed = Vec::new();
    for region in ["us-east-1", "eu-west-1"] {
        let cfg = server.aws_config_in(region).await;
        let ddb = aws_sdk_dynamodb::Client::new(&cfg);
        let kms = aws_sdk_kms::Client::new(&cfg);
        let (key_id, key_arn) = create_key(&kms).await;
        kms.create_alias()
            .alias_name("alias/tables")
            .target_key_id(&key_id)
            .send()
            .await
            .unwrap();

        // fakecloud keeps DynamoDB table names account-wide, so names carry
        // the region.
        let reported = |name: String, key: Option<&'static str>| {
            let ddb = ddb.clone();
            async move {
                let mut sse = aws_sdk_dynamodb::types::SseSpecification::builder()
                    .enabled(true)
                    .sse_type(aws_sdk_dynamodb::types::SseType::Kms);
                if let Some(key) = key {
                    sse = sse.kms_master_key_id(key);
                }
                ddb.create_table()
                    .table_name(&name)
                    .key_schema(
                        aws_sdk_dynamodb::types::KeySchemaElement::builder()
                            .attribute_name("pk")
                            .key_type(aws_sdk_dynamodb::types::KeyType::Hash)
                            .build()
                            .unwrap(),
                    )
                    .attribute_definitions(
                        aws_sdk_dynamodb::types::AttributeDefinition::builder()
                            .attribute_name("pk")
                            .attribute_type(aws_sdk_dynamodb::types::ScalarAttributeType::S)
                            .build()
                            .unwrap(),
                    )
                    .billing_mode(aws_sdk_dynamodb::types::BillingMode::PayPerRequest)
                    .sse_specification(sse.build())
                    .send()
                    .await
                    .unwrap_or_else(|e| panic!("CreateTable {name}: {e:?}"))
                    .table_description
                    .and_then(|t| t.sse_description)
                    .and_then(|s| s.kms_master_key_arn)
                    .expect("KMSMasterKeyArn")
            }
        };
        let default_key = reported(format!("default-key-{region}"), None).await;
        assert!(
            default_key.starts_with(&format!("arn:aws:kms:{region}:")),
            "{region}: {default_key}"
        );
        assert_eq!(
            describe_arn(&kms, "alias/aws/dynamodb").await,
            Some(default_key.clone()),
            "{region}: alias/aws/dynamodb names the key the table reports"
        );
        assert_eq!(
            reported(format!("named-key-{region}"), Some("alias/tables")).await,
            key_arn
        );
        managed.push(default_key);
    }
    assert_ne!(managed[0], managed[1], "one AWS-managed key per region");
}

/// SSE-KMS objects in a eu-west-1 bucket resolve a customer alias in
/// eu-west-1 (not the server's default region) and round-trip.
#[tokio::test]
async fn s3_sse_kms_resolves_alias_in_the_bucket_region() {
    let server = TestServer::start().await;
    let cfg = server.aws_config_in("eu-west-1").await;
    let kms = aws_sdk_kms::Client::new(&cfg);
    let s3 = aws_sdk_s3::Client::from_conf(
        aws_sdk_s3::config::Builder::from(&cfg)
            .force_path_style(true)
            .build(),
    );
    let (key_id, key_arn) = create_key(&kms).await;
    kms.create_alias()
        .alias_name("alias/objects")
        .target_key_id(&key_id)
        .send()
        .await
        .unwrap();
    s3.create_bucket()
        .bucket("regional-sse-bucket")
        .create_bucket_configuration(
            aws_sdk_s3::types::CreateBucketConfiguration::builder()
                .location_constraint(aws_sdk_s3::types::BucketLocationConstraint::EuWest1)
                .build(),
        )
        .send()
        .await
        .expect("CreateBucket in eu-west-1");
    s3.put_object()
        .bucket("regional-sse-bucket")
        .key("obj")
        .body(aws_sdk_s3::primitives::ByteStream::from_static(b"secret"))
        .server_side_encryption(aws_sdk_s3::types::ServerSideEncryption::AwsKms)
        .ssekms_key_id("alias/objects")
        .send()
        .await
        .expect("PutObject under a eu-west-1 alias");
    let head = s3
        .head_object()
        .bucket("regional-sse-bucket")
        .key("obj")
        .send()
        .await
        .unwrap();
    assert_eq!(head.ssekms_key_id(), Some(key_arn.as_str()));
    let got = s3
        .get_object()
        .bucket("regional-sse-bucket")
        .key("obj")
        .send()
        .await
        .unwrap()
        .body
        .collect()
        .await
        .unwrap()
        .into_bytes();
    assert_eq!(got.as_ref(), b"secret");
}
