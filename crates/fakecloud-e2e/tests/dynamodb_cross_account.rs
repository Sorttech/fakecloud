//! Cross-account DynamoDB access through table and stream ARNs.
//!
//! Account A owns the tables; account B names them by ARN. Under
//! `FAKECLOUD_IAM=strict` a cross-account request needs both B's identity
//! policy and the resource-based policy on A's table (or stream). Operations
//! without cross-account support, and ARNs naming another region, find no
//! table at all.

mod helpers;

use aws_credential_types::Credentials;
use aws_sdk_dynamodb::types::{
    AttributeDefinition, AttributeValue, BillingMode, KeySchemaElement, KeyType, KeysAndAttributes,
    Put, PutRequest, ScalarAttributeType, StreamSpecification, StreamViewType, TransactWriteItem,
    WriteRequest,
};
use aws_sdk_dynamodb::Client as DynamoClient;
use aws_sdk_iam::Client as IamClient;
use helpers::TestServer;

const ACCOUNT_A: &str = "123456789012";
const ACCOUNT_B: &str = "222222222222";
const ACCOUNT_C: &str = "333333333333";
const REGION: &str = "us-east-1";

async fn start_strict() -> TestServer {
    TestServer::start_with_env(&[
        ("FAKECLOUD_IAM", "strict"),
        ("FAKECLOUD_VERIFY_SIGV4", "true"),
    ])
    .await
}

async fn config(server: &TestServer, akid: &str, secret: &str) -> aws_config::SdkConfig {
    aws_config::defaults(aws_config::BehaviorVersion::latest())
        .endpoint_url(server.endpoint())
        .region(aws_config::Region::new(REGION))
        .credentials_provider(Credentials::new(
            akid,
            secret,
            None,
            None,
            "dynamodb-x-acct",
        ))
        .load()
        .await
}

/// An administrator's DynamoDB client in `account`, and its SDK config.
async fn admin_in(
    server: &TestServer,
    account: &str,
    name: &str,
) -> (DynamoClient, aws_config::SdkConfig) {
    let (akid, secret) = server.create_admin(account, name).await;
    let cfg = config(server, &akid, &secret).await;
    (DynamoClient::new(&cfg), cfg)
}

fn table_arn(name: &str) -> String {
    format!("arn:aws:dynamodb:{REGION}:{ACCOUNT_A}:table/{name}")
}

async fn create_table(client: &DynamoClient, name: &str) -> String {
    let out = client
        .create_table()
        .table_name(name)
        .key_schema(
            KeySchemaElement::builder()
                .attribute_name("pk")
                .key_type(KeyType::Hash)
                .build()
                .unwrap(),
        )
        .attribute_definitions(
            AttributeDefinition::builder()
                .attribute_name("pk")
                .attribute_type(ScalarAttributeType::S)
                .build()
                .unwrap(),
        )
        .billing_mode(BillingMode::PayPerRequest)
        .stream_specification(
            StreamSpecification::builder()
                .stream_enabled(true)
                .stream_view_type(StreamViewType::NewImage)
                .build()
                .unwrap(),
        )
        .send()
        .await
        .unwrap();
    out.table_description()
        .and_then(|t| t.latest_stream_arn())
        .unwrap_or_default()
        .to_string()
}

async fn put_resource_policy(owner: &DynamoClient, arn: &str, actions: &[&str]) {
    let policy = serde_json::json!({
        "Version": "2012-10-17",
        "Statement": [{
            "Effect": "Allow",
            "Principal": {"AWS": format!("arn:aws:iam::{ACCOUNT_B}:root")},
            "Action": actions,
            "Resource": "*"
        }]
    });
    owner
        .put_resource_policy()
        .resource_arn(arn)
        .policy(policy.to_string())
        .send()
        .await
        .unwrap();
}

fn err_text<T, E: std::fmt::Debug>(result: Result<T, E>) -> String {
    match result {
        Ok(_) => "Ok".to_string(),
        Err(e) => format!("{e:?}"),
    }
}

fn pk(v: &str) -> AttributeValue {
    AttributeValue::S(v.to_string())
}

#[tokio::test]
async fn cross_account_table_access_needs_the_table_resource_policy() {
    let server = start_strict().await;
    let (owner, _) = admin_in(&server, ACCOUNT_A, "admin-a").await;
    let (caller, caller_cfg) = admin_in(&server, ACCOUNT_B, "admin-b").await;
    let (stranger, _) = admin_in(&server, ACCOUNT_C, "admin-c").await;
    create_table(&owner, "Shared").await;
    create_table(&caller, "Mine").await;
    owner
        .put_item()
        .table_name("Shared")
        .item("pk", pk("seed"))
        .send()
        .await
        .unwrap();
    let shared = table_arn("Shared");

    // B's identity policy allows everything, but A's table has no policy.
    let denied = err_text(
        caller
            .get_item()
            .table_name(&shared)
            .key("pk", pk("seed"))
            .send()
            .await,
    );
    assert!(denied.contains("AccessDenied"), "{denied}");

    put_resource_policy(
        &owner,
        &shared,
        &[
            "dynamodb:GetItem",
            "dynamodb:PutItem",
            "dynamodb:Query",
            "dynamodb:BatchGetItem",
            "dynamodb:BatchWriteItem",
            "dynamodb:DescribeTable",
        ],
    )
    .await;

    let got = caller
        .get_item()
        .table_name(&shared)
        .key("pk", pk("seed"))
        .send()
        .await
        .unwrap();
    assert!(got.item().is_some(), "B reads A's item");

    caller
        .put_item()
        .table_name(&shared)
        .item("pk", pk("from-b"))
        .send()
        .await
        .unwrap();
    let seen = owner
        .get_item()
        .table_name("Shared")
        .key("pk", pk("from-b"))
        .send()
        .await
        .unwrap();
    assert!(seen.item().is_some(), "B's write lands in A's table");
    let own = caller
        .get_item()
        .table_name("Mine")
        .key("pk", pk("from-b"))
        .send()
        .await
        .unwrap();
    assert!(own.item().is_none(), "and not in B's own table");

    let described = caller
        .describe_table()
        .table_name(&shared)
        .send()
        .await
        .unwrap();
    assert_eq!(
        described.table().and_then(|t| t.table_arn()),
        Some(shared.as_str())
    );

    let queried = caller
        .query()
        .table_name(&shared)
        .key_condition_expression("pk = :p")
        .expression_attribute_values(":p", pk("seed"))
        .send()
        .await
        .unwrap();
    assert_eq!(queried.count(), 1);

    // A batch spanning B's own table and A's.
    caller
        .batch_write_item()
        .request_items(
            "Mine",
            vec![WriteRequest::builder()
                .put_request(PutRequest::builder().item("pk", pk("k")).build().unwrap())
                .build()],
        )
        .request_items(
            &shared,
            vec![WriteRequest::builder()
                .put_request(PutRequest::builder().item("pk", pk("k")).build().unwrap())
                .build()],
        )
        .send()
        .await
        .unwrap();
    let batch = caller
        .batch_get_item()
        .request_items(
            "Mine",
            KeysAndAttributes::builder()
                .keys([("pk".to_string(), pk("k"))].into())
                .build()
                .unwrap(),
        )
        .request_items(
            &shared,
            KeysAndAttributes::builder()
                .keys([("pk".to_string(), pk("k"))].into())
                .build()
                .unwrap(),
        )
        .send()
        .await
        .unwrap();
    let responses = batch.responses().unwrap();
    assert_eq!(responses["Mine"].len(), 1);
    assert_eq!(responses[&shared].len(), 1);

    // A transaction spanning both accounts' tables.
    caller
        .transact_write_items()
        .transact_items(
            TransactWriteItem::builder()
                .put(
                    Put::builder()
                        .table_name("Mine")
                        .item("pk", pk("t"))
                        .build()
                        .unwrap(),
                )
                .build(),
        )
        .transact_items(
            TransactWriteItem::builder()
                .put(
                    Put::builder()
                        .table_name(&shared)
                        .item("pk", pk("t"))
                        .build()
                        .unwrap(),
                )
                .build(),
        )
        .send()
        .await
        .unwrap();
    let committed = owner
        .get_item()
        .table_name("Shared")
        .key("pk", pk("t"))
        .send()
        .await
        .unwrap();
    assert!(committed.item().is_some());

    // The policy does not grant DeleteItem.
    let denied = err_text(
        caller
            .delete_item()
            .table_name(&shared)
            .key("pk", pk("seed"))
            .send()
            .await,
    );
    assert!(denied.contains("AccessDenied"), "{denied}");

    // An account the policy does not name is denied.
    let denied = err_text(
        stranger
            .get_item()
            .table_name(&shared)
            .key("pk", pk("seed"))
            .send()
            .await,
    );
    assert!(denied.contains("AccessDenied"), "{denied}");

    // A user in B without an identity policy is denied: cross-account access
    // needs both policies.
    let iam = IamClient::new(&caller_cfg);
    iam.create_user().user_name("bare").send().await.unwrap();
    let key = iam
        .create_access_key()
        .user_name("bare")
        .send()
        .await
        .unwrap();
    let key = key.access_key().unwrap();
    let bare =
        DynamoClient::new(&config(&server, key.access_key_id(), key.secret_access_key()).await);
    let denied = err_text(
        bare.get_item()
            .table_name(&shared)
            .key("pk", pk("seed"))
            .send()
            .await,
    );
    assert!(denied.contains("AccessDenied"), "{denied}");
}

#[tokio::test]
async fn unsupported_operations_and_other_regions_find_no_table() {
    let server = start_strict().await;
    let (owner, _) = admin_in(&server, ACCOUNT_A, "admin-a").await;
    let (caller, _) = admin_in(&server, ACCOUNT_B, "admin-b").await;
    create_table(&owner, "Shared").await;
    let shared = table_arn("Shared");
    put_resource_policy(&owner, &shared, &["dynamodb:*"]).await;

    let backup = err_text(
        caller
            .create_backup()
            .table_name(&shared)
            .backup_name("b")
            .send()
            .await,
    );
    assert!(backup.contains("TableNotFoundException"), "{backup}");

    let ttl = err_text(
        caller
            .describe_time_to_live()
            .table_name(&shared)
            .send()
            .await,
    );
    assert!(ttl.contains("ResourceNotFoundException"), "{ttl}");

    let partiql = err_text(
        caller
            .execute_statement()
            .statement(format!("SELECT * FROM \"{shared}\""))
            .send()
            .await,
    );
    assert!(partiql.contains("ResourceNotFoundException"), "{partiql}");

    let other_region = format!("arn:aws:dynamodb:us-west-2:{ACCOUNT_A}:table/Shared");
    let region = err_text(
        caller
            .get_item()
            .table_name(&other_region)
            .key("pk", pk("x"))
            .send()
            .await,
    );
    assert!(region.contains("ResourceNotFoundException"), "{region}");

    // B does not see A's table in its own listing.
    let tables = caller.list_tables().send().await.unwrap();
    assert!(tables.table_names().is_empty());
}

#[tokio::test]
async fn cross_account_stream_reads_need_the_stream_resource_policy() {
    let server = start_strict().await;
    let (owner, _) = admin_in(&server, ACCOUNT_A, "admin-a").await;
    let (_, caller_cfg) = admin_in(&server, ACCOUNT_B, "admin-b").await;
    let stream_arn = create_table(&owner, "Shared").await;
    assert!(!stream_arn.is_empty());
    owner
        .put_item()
        .table_name("Shared")
        .item("pk", pk("seed"))
        .send()
        .await
        .unwrap();
    let streams = aws_sdk_dynamodbstreams::Client::new(&caller_cfg);

    let denied = err_text(
        streams
            .describe_stream()
            .stream_arn(&stream_arn)
            .send()
            .await,
    );
    assert!(denied.contains("AccessDenied"), "{denied}");

    // A table policy does not cover the stream.
    put_resource_policy(&owner, &table_arn("Shared"), &["dynamodb:*"]).await;
    let denied = err_text(
        streams
            .describe_stream()
            .stream_arn(&stream_arn)
            .send()
            .await,
    );
    assert!(denied.contains("AccessDenied"), "{denied}");

    put_resource_policy(
        &owner,
        &stream_arn,
        &[
            "dynamodb:DescribeStream",
            "dynamodb:GetShardIterator",
            "dynamodb:GetRecords",
        ],
    )
    .await;
    let described = streams
        .describe_stream()
        .stream_arn(&stream_arn)
        .send()
        .await
        .unwrap();
    let shard = described
        .stream_description()
        .unwrap()
        .shards()
        .first()
        .and_then(|s| s.shard_id())
        .unwrap()
        .to_string();
    let iterator = streams
        .get_shard_iterator()
        .stream_arn(&stream_arn)
        .shard_id(shard)
        .shard_iterator_type(aws_sdk_dynamodbstreams::types::ShardIteratorType::TrimHorizon)
        .send()
        .await
        .unwrap();
    let records = streams
        .get_records()
        .shard_iterator(iterator.shard_iterator().unwrap())
        .send()
        .await
        .unwrap();
    assert_eq!(records.records().len(), 1);
}
