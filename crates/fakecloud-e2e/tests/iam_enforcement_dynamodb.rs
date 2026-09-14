//! IAM enforcement for DynamoDB and DynamoDB Streams.
//!
//! Each test starts fakecloud with `FAKECLOUD_IAM=strict`, seeds tables with
//! the root-bypass `test` credentials, gives a user an inline policy, and
//! checks what that user's own credentials may do.

mod helpers;

use aws_credential_types::Credentials;
use aws_sdk_dynamodb::types::{
    AttributeDefinition, AttributeValue, BillingMode, GlobalSecondaryIndex, KeySchemaElement,
    KeyType, Projection, ProjectionType, Put, PutRequest, ScalarAttributeType, StreamSpecification,
    StreamViewType, Tag, TransactWriteItem, WriteRequest,
};
use aws_sdk_dynamodb::Client as DynamoClient;
use aws_sdk_iam::Client as IamClient;
use helpers::TestServer;

const ACCOUNT: &str = "123456789012";
const REGION: &str = "us-east-1";

async fn start_strict() -> TestServer {
    TestServer::start_with_env(&[
        ("FAKECLOUD_IAM", "strict"),
        ("FAKECLOUD_VERIFY_SIGV4", "true"),
    ])
    .await
}

async fn sdk_config_with(server: &TestServer, akid: &str, secret: &str) -> aws_config::SdkConfig {
    aws_config::defaults(aws_config::BehaviorVersion::latest())
        .endpoint_url(server.endpoint())
        .region(aws_config::Region::new(REGION))
        .credentials_provider(Credentials::new(
            akid,
            secret,
            None,
            None,
            "fakecloud-dynamodb-iam",
        ))
        .load()
        .await
}

async fn admin(server: &TestServer) -> DynamoClient {
    DynamoClient::new(&sdk_config_with(server, "test", "test").await)
}

/// A user whose only permissions are `policy`, and a DynamoDB client signed
/// with that user's credentials.
async fn user_with_policy(server: &TestServer, name: &str, policy: &str) -> DynamoClient {
    let boot = sdk_config_with(server, "test", "test").await;
    let iam = IamClient::new(&boot);
    iam.create_user().user_name(name).send().await.unwrap();
    let key = iam
        .create_access_key()
        .user_name(name)
        .send()
        .await
        .unwrap();
    let key = key.access_key().unwrap();
    iam.put_user_policy()
        .user_name(name)
        .policy_name("inline")
        .policy_document(policy)
        .send()
        .await
        .unwrap();
    DynamoClient::new(&sdk_config_with(server, key.access_key_id(), key.secret_access_key()).await)
}

fn table_arn(name: &str) -> String {
    format!("arn:aws:dynamodb:{REGION}:{ACCOUNT}:table/{name}")
}

fn allow(actions: &[&str], resources: &[String]) -> String {
    serde_json::json!({
        "Version": "2012-10-17",
        "Statement": [{"Effect": "Allow", "Action": actions, "Resource": resources}]
    })
    .to_string()
}

async fn create_table(client: &DynamoClient, name: &str) {
    client
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
        .attribute_definitions(
            AttributeDefinition::builder()
                .attribute_name("g")
                .attribute_type(ScalarAttributeType::S)
                .build()
                .unwrap(),
        )
        .global_secondary_indexes(
            GlobalSecondaryIndex::builder()
                .index_name("by-g")
                .key_schema(
                    KeySchemaElement::builder()
                        .attribute_name("g")
                        .key_type(KeyType::Hash)
                        .build()
                        .unwrap(),
                )
                .projection(
                    Projection::builder()
                        .projection_type(ProjectionType::All)
                        .build(),
                )
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
}

fn denied<E: std::fmt::Debug>(result: Result<impl std::fmt::Debug, E>) -> bool {
    match result {
        Ok(_) => false,
        Err(e) => format!("{e:?}").contains("AccessDenied"),
    }
}

#[tokio::test]
async fn dynamodb_requires_a_policy_under_strict_enforcement() {
    let server = start_strict().await;
    create_table(&admin(&server).await, "Orders").await;
    let nobody = user_with_policy(
        &server,
        "nobody",
        &allow(&["sqs:ListQueues"], &["*".to_string()]),
    )
    .await;

    assert!(denied(nobody.list_tables().send().await));
    assert!(denied(
        nobody
            .get_item()
            .table_name("Orders")
            .key("pk", AttributeValue::S("a".into()))
            .send()
            .await
    ));
}

/// A policy scoped to one action on one table allows exactly that.
#[tokio::test]
async fn item_actions_are_scoped_to_the_table_and_action() {
    let server = start_strict().await;
    let admin = admin(&server).await;
    create_table(&admin, "Orders").await;
    create_table(&admin, "Customers").await;
    let reader = user_with_policy(
        &server,
        "reader",
        &allow(&["dynamodb:GetItem"], &[table_arn("Orders")]),
    )
    .await;

    reader
        .get_item()
        .table_name("Orders")
        .key("pk", AttributeValue::S("a".into()))
        .send()
        .await
        .expect("GetItem on the allowed table");
    assert!(denied(
        reader
            .put_item()
            .table_name("Orders")
            .item("pk", AttributeValue::S("a".into()))
            .send()
            .await
    ));
    assert!(denied(
        reader
            .get_item()
            .table_name("Customers")
            .key("pk", AttributeValue::S("a".into()))
            .send()
            .await
    ));
    // The table's ARN authorizes the same as its name.
    reader
        .get_item()
        .table_name(table_arn("Orders"))
        .key("pk", AttributeValue::S("a".into()))
        .send()
        .await
        .expect("GetItem by table ARN");
}

/// A Query on an index is authorized against the index's ARN, not the
/// table's.
#[tokio::test]
async fn index_queries_are_authorized_against_the_index() {
    let server = start_strict().await;
    create_table(&admin(&server).await, "Orders").await;
    let table_only = user_with_policy(
        &server,
        "table-only",
        &allow(&["dynamodb:Query"], &[table_arn("Orders")]),
    )
    .await;
    let query_index = |client: DynamoClient| async move {
        client
            .query()
            .table_name("Orders")
            .index_name("by-g")
            .key_condition_expression("g = :g")
            .expression_attribute_values(":g", AttributeValue::S("x".into()))
            .send()
            .await
    };
    assert!(denied(query_index(table_only).await));

    let with_index = user_with_policy(
        &server,
        "with-index",
        &allow(
            &["dynamodb:Query"],
            &[
                table_arn("Orders"),
                format!("{}/index/*", table_arn("Orders")),
            ],
        ),
    )
    .await;
    query_index(with_index)
        .await
        .expect("Query on an allowed index");
}

/// A batch or transaction needs the permission on every table it touches:
/// being allowed on one of them is not enough.
#[tokio::test]
async fn batches_and_transactions_need_every_table() {
    let server = start_strict().await;
    let admin = admin(&server).await;
    create_table(&admin, "Orders").await;
    create_table(&admin, "Customers").await;

    let orders_only = user_with_policy(
        &server,
        "orders-only",
        &allow(
            &["dynamodb:PutItem", "dynamodb:BatchWriteItem"],
            &[table_arn("Orders")],
        ),
    )
    .await;
    let both = user_with_policy(
        &server,
        "both",
        &allow(
            &["dynamodb:PutItem", "dynamodb:BatchWriteItem"],
            &[table_arn("Orders"), table_arn("Customers")],
        ),
    )
    .await;

    let put = |table: &str, pk: &str| {
        TransactWriteItem::builder()
            .put(
                Put::builder()
                    .table_name(table)
                    .item("pk", AttributeValue::S(pk.into()))
                    .build()
                    .unwrap(),
            )
            .build()
    };
    assert!(denied(
        orders_only
            .transact_write_items()
            .transact_items(put("Orders", "a"))
            .transact_items(put("Customers", "a"))
            .send()
            .await
    ));
    both.transact_write_items()
        .transact_items(put("Orders", "a"))
        .transact_items(put("Customers", "a"))
        .send()
        .await
        .expect("transaction allowed on both tables");

    let write = |pk: &str| {
        WriteRequest::builder()
            .put_request(
                PutRequest::builder()
                    .item("pk", AttributeValue::S(pk.into()))
                    .build()
                    .unwrap(),
            )
            .build()
    };
    assert!(denied(
        orders_only
            .batch_write_item()
            .request_items("Orders", vec![write("b")])
            .request_items("Customers", vec![write("b")])
            .send()
            .await
    ));
    orders_only
        .batch_write_item()
        .request_items("Orders", vec![write("b")])
        .send()
        .await
        .expect("batch on the allowed table only");
    // Nothing the denied requests carried was written.
    let scan = admin.scan().table_name("Customers").send().await.unwrap();
    let pks: Vec<&str> = scan
        .items()
        .iter()
        .map(|i| i["pk"].as_s().unwrap().as_str())
        .collect();
    assert_eq!(pks, ["a"]);
}

/// PartiQL statements are authorized by their verb.
#[tokio::test]
async fn partiql_statements_need_their_partiql_action() {
    let server = start_strict().await;
    create_table(&admin(&server).await, "Orders").await;
    let selector = user_with_policy(
        &server,
        "selector",
        &allow(&["dynamodb:PartiQLSelect"], &[table_arn("Orders")]),
    )
    .await;

    selector
        .execute_statement()
        .statement("SELECT * FROM \"Orders\"")
        .send()
        .await
        .expect("SELECT allowed");
    assert!(denied(
        selector
            .execute_statement()
            .statement("INSERT INTO \"Orders\" VALUE {'pk': 'a'}")
            .send()
            .await
    ));
    // The data-plane action does not grant the PartiQL one, or the reverse.
    assert!(denied(selector.scan().table_name("Orders").send().await));
}

/// `aws:ResourceTag/*` conditions see the table's tags, and CreateTable with
/// tags also needs `dynamodb:TagResource`.
#[tokio::test]
async fn tag_conditions_and_create_table_tags() {
    let server = start_strict().await;
    let admin = admin(&server).await;
    create_table(&admin, "Tagged").await;
    create_table(&admin, "Untagged").await;
    admin
        .tag_resource()
        .resource_arn(table_arn("Tagged"))
        .tags(
            Tag::builder()
                .key("team")
                .value("payments")
                .build()
                .unwrap(),
        )
        .send()
        .await
        .unwrap();

    let policy = serde_json::json!({
        "Version": "2012-10-17",
        "Statement": [
            {
                "Effect": "Allow",
                "Action": "dynamodb:GetItem",
                "Resource": "*",
                "Condition": {"StringEquals": {"aws:ResourceTag/team": "payments"}}
            },
            {
                "Effect": "Allow",
                "Action": "dynamodb:CreateTable",
                "Resource": "*"
            }
        ]
    })
    .to_string();
    let payments = user_with_policy(&server, "payments", &policy).await;
    let get = |client: &DynamoClient, table: &str| {
        client
            .get_item()
            .table_name(table)
            .key("pk", AttributeValue::S("a".into()))
            .send()
    };
    get(&payments, "Tagged").await.expect("tag matches");
    assert!(denied(get(&payments, "Untagged").await));

    let create = |name: &str, tagged: bool| {
        let mut req = payments
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
            .billing_mode(BillingMode::PayPerRequest);
        if tagged {
            req = req.tags(
                Tag::builder()
                    .key("team")
                    .value("payments")
                    .build()
                    .unwrap(),
            );
        }
        req.send()
    };
    create("Plain", false).await.expect("CreateTable allowed");
    assert!(
        denied(create("WithTags", true).await),
        "CreateTable with Tags also needs dynamodb:TagResource"
    );
}

/// DynamoDB Streams operations are authorized against the stream ARN.
#[tokio::test]
async fn streams_are_authorized_against_the_stream() {
    let server = start_strict().await;
    let admin = admin(&server).await;
    create_table(&admin, "Orders").await;
    let stream_arn = admin
        .describe_table()
        .table_name("Orders")
        .send()
        .await
        .unwrap()
        .table()
        .unwrap()
        .latest_stream_arn()
        .unwrap()
        .to_string();

    let boot = sdk_config_with(&server, "test", "test").await;
    let iam = IamClient::new(&boot);
    iam.create_user()
        .user_name("streamer")
        .send()
        .await
        .unwrap();
    let key = iam
        .create_access_key()
        .user_name("streamer")
        .send()
        .await
        .unwrap();
    let key = key.access_key().unwrap();
    iam.put_user_policy()
        .user_name("streamer")
        .policy_name("inline")
        .policy_document(allow(
            &["dynamodb:DescribeStream"],
            std::slice::from_ref(&stream_arn),
        ))
        .send()
        .await
        .unwrap();
    let streams = aws_sdk_dynamodbstreams::Client::new(
        &sdk_config_with(&server, key.access_key_id(), key.secret_access_key()).await,
    );

    let described = streams
        .describe_stream()
        .stream_arn(&stream_arn)
        .send()
        .await
        .expect("DescribeStream allowed");
    let shard = described.stream_description().unwrap().shards()[0]
        .shard_id()
        .unwrap()
        .to_string();
    assert!(denied(
        streams
            .get_shard_iterator()
            .stream_arn(&stream_arn)
            .shard_id(shard)
            .shard_iterator_type(aws_sdk_dynamodbstreams::types::ShardIteratorType::TrimHorizon)
            .send()
            .await
    ));
    assert!(denied(streams.list_streams().send().await));
}
