//! L4 — DynamoDB PartiQL WHERE-clause + INSERT validation + stream
//! emission end-to-end coverage. Drives the live AWS Rust SDK against
//! `ExecuteStatement` so any drift between our PartiQL evaluator and
//! the SDK's wire format surfaces immediately. The unit tests in
//! `crates/fakecloud-dynamodb/src/service/tests.rs` cover the same
//! behaviors against the in-process service; this file proves the
//! flow round-trips through the real HTTP layer.

mod helpers;

use aws_sdk_dynamodb::types::{
    AttributeDefinition, AttributeValue, BillingMode, KeySchemaElement, KeyType,
    ScalarAttributeType, StreamSpecification, StreamViewType,
};
use helpers::TestServer;

async fn create_streamed_table(ddb: &aws_sdk_dynamodb::Client, name: &str) {
    ddb.create_table()
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
                .stream_view_type(StreamViewType::NewAndOldImages)
                .build()
                .unwrap(),
        )
        .send()
        .await
        .unwrap();
}

async fn put_row(ddb: &aws_sdk_dynamodb::Client, table: &str, pk: &str, n: i64, s: &str) {
    ddb.put_item()
        .table_name(table)
        .item("pk", AttributeValue::S(pk.into()))
        .item("n", AttributeValue::N(n.to_string()))
        .item("s", AttributeValue::S(s.into()))
        .send()
        .await
        .unwrap();
}

async fn select_pks(ddb: &aws_sdk_dynamodb::Client, statement: &str) -> Vec<String> {
    let resp = ddb
        .execute_statement()
        .statement(statement)
        .send()
        .await
        .unwrap();
    let mut pks: Vec<String> = resp
        .items()
        .iter()
        .map(|it| it.get("pk").unwrap().as_s().unwrap().clone())
        .collect();
    pks.sort();
    pks
}

#[tokio::test]
async fn ddb_partiql_select_n_gt_5_and_s_like_foo() {
    // L4 spec example: SELECT * FROM "T" WHERE n > 5 AND s LIKE 'foo%'
    // returns the right subset.
    let server = TestServer::start().await;
    let ddb = server.dynamodb_client().await;
    let table = "L4PartiqlSelectAnd";
    create_streamed_table(&ddb, table).await;

    put_row(&ddb, table, "k1", 1, "foobar").await; // n too low
    put_row(&ddb, table, "k2", 6, "foobar").await; // matches
    put_row(&ddb, table, "k3", 6, "barfoo").await; // wrong prefix
    put_row(&ddb, table, "k4", 9, "fooz").await; // matches

    let pks = select_pks(
        &ddb,
        &format!("SELECT * FROM \"{table}\" WHERE n > 5 AND s LIKE 'foo%'"),
    )
    .await;
    assert_eq!(pks, vec!["k2", "k4"]);
}

#[tokio::test]
async fn ddb_partiql_select_or_not_parens() {
    // L4 spec: WHERE composition with AND/OR/NOT and parens. Each
    // sub-form exercises a different branch of the recursive parser.
    let server = TestServer::start().await;
    let ddb = server.dynamodb_client().await;
    let table = "L4PartiqlSelectOrNotParens";
    create_streamed_table(&ddb, table).await;

    for (pk, n) in [("a", 10_i64), ("b", 20), ("c", 30), ("d", 40)] {
        ddb.put_item()
            .table_name(table)
            .item("pk", AttributeValue::S(pk.into()))
            .item("n", AttributeValue::N(n.to_string()))
            .send()
            .await
            .unwrap();
    }

    // OR — match the lower and upper edges.
    assert_eq!(
        select_pks(
            &ddb,
            &format!("SELECT * FROM \"{table}\" WHERE n < 15 OR n > 35")
        )
        .await,
        vec!["a", "d"]
    );
    // NOT inverts a comparator.
    assert_eq!(
        select_pks(
            &ddb,
            &format!("SELECT * FROM \"{table}\" WHERE NOT n >= 30")
        )
        .await,
        vec!["a", "b"]
    );
    // Parens force OR to bind tighter than AND.
    assert_eq!(
        select_pks(
            &ddb,
            &format!("SELECT * FROM \"{table}\" WHERE (n < 15 OR n > 35) AND attribute_exists(pk)"),
        )
        .await,
        vec!["a", "d"]
    );
}

#[tokio::test]
async fn ddb_partiql_insert_missing_sort_key_validation() {
    // L4 spec: INSERT with missing sort key returns ValidationException.
    let server = TestServer::start().await;
    let ddb = server.dynamodb_client().await;
    let table = "L4PartiqlMissingSk";

    ddb.create_table()
        .table_name(table)
        .key_schema(
            KeySchemaElement::builder()
                .attribute_name("pk")
                .key_type(KeyType::Hash)
                .build()
                .unwrap(),
        )
        .key_schema(
            KeySchemaElement::builder()
                .attribute_name("sk")
                .key_type(KeyType::Range)
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
                .attribute_name("sk")
                .attribute_type(ScalarAttributeType::S)
                .build()
                .unwrap(),
        )
        .billing_mode(BillingMode::PayPerRequest)
        .send()
        .await
        .unwrap();

    let err = ddb
        .execute_statement()
        .statement(format!("INSERT INTO \"{table}\" VALUE {{'pk': 'a'}}"))
        .send()
        .await
        .expect_err("insert without sort key must fail");
    let msg = format!("{:?}", err.into_service_error());
    // PartiQL INSERT with a missing required key is a ValidationException,
    // matching AWS — the prior remap to ResourceNotFoundException returned the
    // wrong __type to clients and is no longer applied.
    assert!(msg.contains("ValidationException"), "got {msg}");
    assert!(msg.contains("Missing the key sk"), "got {msg}");
}

#[tokio::test]
async fn ddb_partiql_update_emits_stream_record() {
    // L4 spec: UPDATE statement on a stream-enabled table emits a
    // MODIFY stream record visible via the Streams data plane.
    let server = TestServer::start().await;
    let ddb = server.dynamodb_client().await;
    let streams = server.dynamodb_streams_client().await;
    let table = "L4PartiqlUpdateStream";

    create_streamed_table(&ddb, table).await;
    put_row(&ddb, table, "u1", 1, "x").await;

    let stream_arn = ddb
        .describe_table()
        .table_name(table)
        .send()
        .await
        .unwrap()
        .table()
        .unwrap()
        .latest_stream_arn()
        .unwrap()
        .to_string();
    let shard_id = streams
        .describe_stream()
        .stream_arn(&stream_arn)
        .send()
        .await
        .unwrap()
        .stream_description()
        .unwrap()
        .shards()
        .first()
        .unwrap()
        .shard_id()
        .unwrap()
        .to_string();

    // Snapshot baseline shard length so we count only what the UPDATE
    // adds on top of the seeding PutItem.
    let baseline_iter = streams
        .get_shard_iterator()
        .stream_arn(&stream_arn)
        .shard_id(&shard_id)
        .shard_iterator_type(aws_sdk_dynamodbstreams::types::ShardIteratorType::TrimHorizon)
        .send()
        .await
        .unwrap();
    let baseline_len = streams
        .get_records()
        .shard_iterator(baseline_iter.shard_iterator().unwrap())
        .send()
        .await
        .unwrap()
        .records()
        .len();

    ddb.execute_statement()
        .statement(format!("UPDATE \"{table}\" SET n = 99 WHERE pk = 'u1'"))
        .send()
        .await
        .unwrap();

    let after_iter = streams
        .get_shard_iterator()
        .stream_arn(&stream_arn)
        .shard_id(&shard_id)
        .shard_iterator_type(aws_sdk_dynamodbstreams::types::ShardIteratorType::TrimHorizon)
        .send()
        .await
        .unwrap();
    let after = streams
        .get_records()
        .shard_iterator(after_iter.shard_iterator().unwrap())
        .send()
        .await
        .unwrap();
    assert_eq!(
        after.records().len(),
        baseline_len + 1,
        "UPDATE must emit exactly one stream record"
    );
    assert_eq!(
        after
            .records()
            .last()
            .unwrap()
            .event_name()
            .unwrap()
            .as_str(),
        "MODIFY",
    );
}

#[tokio::test]
async fn ddb_partiql_delete_emits_stream_record() {
    // L4 spec: DELETE statement on a stream-enabled table emits a
    // REMOVE stream record visible via the Streams data plane.
    let server = TestServer::start().await;
    let ddb = server.dynamodb_client().await;
    let streams = server.dynamodb_streams_client().await;
    let table = "L4PartiqlDeleteStream";

    create_streamed_table(&ddb, table).await;
    put_row(&ddb, table, "d1", 1, "x").await;

    let stream_arn = ddb
        .describe_table()
        .table_name(table)
        .send()
        .await
        .unwrap()
        .table()
        .unwrap()
        .latest_stream_arn()
        .unwrap()
        .to_string();
    let shard_id = streams
        .describe_stream()
        .stream_arn(&stream_arn)
        .send()
        .await
        .unwrap()
        .stream_description()
        .unwrap()
        .shards()
        .first()
        .unwrap()
        .shard_id()
        .unwrap()
        .to_string();

    let baseline_iter = streams
        .get_shard_iterator()
        .stream_arn(&stream_arn)
        .shard_id(&shard_id)
        .shard_iterator_type(aws_sdk_dynamodbstreams::types::ShardIteratorType::TrimHorizon)
        .send()
        .await
        .unwrap();
    let baseline_len = streams
        .get_records()
        .shard_iterator(baseline_iter.shard_iterator().unwrap())
        .send()
        .await
        .unwrap()
        .records()
        .len();

    ddb.execute_statement()
        .statement(format!("DELETE FROM \"{table}\" WHERE pk = 'd1'"))
        .send()
        .await
        .unwrap();

    let after_iter = streams
        .get_shard_iterator()
        .stream_arn(&stream_arn)
        .shard_id(&shard_id)
        .shard_iterator_type(aws_sdk_dynamodbstreams::types::ShardIteratorType::TrimHorizon)
        .send()
        .await
        .unwrap();
    let after = streams
        .get_records()
        .shard_iterator(after_iter.shard_iterator().unwrap())
        .send()
        .await
        .unwrap();
    assert_eq!(
        after.records().len(),
        baseline_len + 1,
        "DELETE must emit exactly one stream record"
    );
    assert_eq!(
        after
            .records()
            .last()
            .unwrap()
            .event_name()
            .unwrap()
            .as_str(),
        "REMOVE",
    );
}

// bug-audit 2026-06-27, T1.2: positional `?` parameters bind in textual order.
// In `UPDATE t SET x=? WHERE y=?`, SET must consume parameters[0] and WHERE
// parameters[1]; they were previously swapped.
#[tokio::test]
async fn ddb_partiql_update_positional_param_order() {
    let server = TestServer::start().await;
    let ddb = server.dynamodb_client().await;
    create_streamed_table(&ddb, "ParamOrder").await;
    put_row(&ddb, "ParamOrder", "r1", 1, "orig").await;

    ddb.execute_statement()
        .statement("UPDATE \"ParamOrder\" SET s=? WHERE pk=?")
        .parameters(AttributeValue::S("updated".into()))
        .parameters(AttributeValue::S("r1".into()))
        .send()
        .await
        .expect("parameterized update");

    let got = ddb
        .get_item()
        .table_name("ParamOrder")
        .key("pk", AttributeValue::S("r1".into()))
        .send()
        .await
        .unwrap();
    let item = got.item().expect("row exists");
    assert_eq!(
        item.get("s")
            .and_then(|v| v.as_s().ok())
            .map(String::as_str),
        Some("updated"),
        "SET bound parameters[0] and WHERE matched on parameters[1]"
    );
}

// bug-audit 2026-06-27, T1.3: REMOVE of a nested map path must delete the
// nested attribute, not a literal top-level key (which was a silent no-op).
#[tokio::test]
async fn ddb_update_remove_nested_path() {
    let server = TestServer::start().await;
    let ddb = server.dynamodb_client().await;
    create_streamed_table(&ddb, "NestedRemove").await;

    ddb.put_item()
        .table_name("NestedRemove")
        .item("pk", AttributeValue::S("r1".into()))
        .item(
            "profile",
            AttributeValue::M(
                [
                    ("first".to_string(), AttributeValue::S("a".into())),
                    ("middle".to_string(), AttributeValue::S("b".into())),
                ]
                .into(),
            ),
        )
        .send()
        .await
        .unwrap();

    ddb.update_item()
        .table_name("NestedRemove")
        .key("pk", AttributeValue::S("r1".into()))
        .update_expression("REMOVE profile.middle")
        .send()
        .await
        .expect("remove nested");

    let got = ddb
        .get_item()
        .table_name("NestedRemove")
        .key("pk", AttributeValue::S("r1".into()))
        .send()
        .await
        .unwrap();
    let profile = got.item().unwrap().get("profile").unwrap().as_m().unwrap();
    assert!(!profile.contains_key("middle"), "nested key removed");
    assert!(profile.contains_key("first"), "sibling key kept");
}

// bug-audit 2026-06-27, T1.12: PartiQL numeric comparisons must use the exact
// decimal value, not f64 (which rounds past 2^53 so two distinct large ints
// compare equal).
#[tokio::test]
async fn ddb_partiql_large_integer_comparison_is_exact() {
    let server = TestServer::start().await;
    let ddb = server.dynamodb_client().await;
    create_streamed_table(&ddb, "BigInt").await;

    // 2^53 + 1 — indistinguishable from 2^53 under f64.
    ddb.execute_statement()
        .statement("INSERT INTO \"BigInt\" value {'pk':'a','n':9007199254740993}")
        .send()
        .await
        .unwrap();

    let resp = ddb
        .execute_statement()
        .statement("SELECT pk FROM \"BigInt\" WHERE n > 9007199254740992")
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.items().len(),
        1,
        "9007199254740993 > 9007199254740992 holds with exact decimal comparison"
    );
}

// bug-audit 2026-06-27, T1.12: ExecuteStatement (PartiQL SELECT) must honor
// Limit and return/accept NextToken for pagination.
#[tokio::test]
async fn ddb_partiql_execute_statement_paginates() {
    let server = TestServer::start().await;
    let ddb = server.dynamodb_client().await;
    create_streamed_table(&ddb, "Paged").await;
    for (i, pk) in ["p1", "p2", "p3"].iter().enumerate() {
        put_row(&ddb, "Paged", pk, i as i64, "x").await;
    }

    let page1 = ddb
        .execute_statement()
        .statement("SELECT pk FROM \"Paged\"")
        .limit(2)
        .send()
        .await
        .unwrap();
    assert_eq!(page1.items().len(), 2, "Limit caps the page");
    let token = page1.next_token().expect("more results -> NextToken");

    let page2 = ddb
        .execute_statement()
        .statement("SELECT pk FROM \"Paged\"")
        .limit(2)
        .next_token(token)
        .send()
        .await
        .unwrap();
    assert_eq!(page2.items().len(), 1, "remaining item on page 2");
    assert!(page2.next_token().is_none(), "last page has no NextToken");
}

// NextToken resumes a SELECT after the last row returned, by key. As an
// offset into the result set it skipped rows whenever rows a page had already
// returned were deleted before the next page was fetched.
#[tokio::test]
async fn ddb_partiql_execute_statement_pages_survive_deletes() {
    let server = TestServer::start().await;
    let ddb = server.dynamodb_client().await;
    create_streamed_table(&ddb, "PagedDrain").await;
    let all: Vec<String> = (0..12).map(|i| format!("r{i:02}")).collect();
    for (i, pk) in all.iter().enumerate() {
        put_row(&ddb, "PagedDrain", pk, i as i64, "x").await;
    }

    let mut delivered: Vec<String> = Vec::new();
    let mut token: Option<String> = None;
    loop {
        let page = ddb
            .execute_statement()
            .statement("SELECT * FROM \"PagedDrain\"")
            .limit(5)
            .set_next_token(token.clone())
            .send()
            .await
            .unwrap();
        let pks: Vec<String> = page
            .items()
            .iter()
            .map(|item| item["pk"].as_s().unwrap().clone())
            .collect();
        for pk in &pks {
            ddb.execute_statement()
                .statement(format!("DELETE FROM \"PagedDrain\" WHERE pk = '{pk}'"))
                .send()
                .await
                .unwrap();
        }
        delivered.extend(pks);
        token = page.next_token().map(str::to_string);
        if token.is_none() {
            break;
        }
    }

    delivered.sort();
    assert_eq!(delivered, all, "every row is returned exactly once");
}

// bug-hunt 2026-07-01, finding 2: a PartiQL SELECT whose string literal
// contains a `<` (or other operator char) must not be split inside the quotes
// — the operator scan skips single-quoted spans, so the equality still parses
// and matches the stored row.
#[tokio::test]
async fn ddb_partiql_select_operator_char_inside_string_literal() {
    let server = TestServer::start().await;
    let ddb = server.dynamodb_client().await;
    create_streamed_table(&ddb, "Urls").await;

    let url = "https://x?a<b";
    ddb.put_item()
        .table_name("Urls")
        .item("pk", AttributeValue::S("row1".into()))
        .item("s", AttributeValue::S(url.into()))
        .send()
        .await
        .unwrap();

    let resp = ddb
        .execute_statement()
        .statement(format!("SELECT pk FROM \"Urls\" WHERE s = '{url}'"))
        .send()
        .await
        .expect("SELECT with a `<` inside the string literal must succeed");
    let pks: Vec<String> = resp
        .items()
        .iter()
        .map(|it| it.get("pk").unwrap().as_s().unwrap().clone())
        .collect();
    assert_eq!(pks, vec!["row1".to_string()]);
}

fn service_error(err: impl aws_sdk_dynamodb::error::ProvideErrorMetadata) -> (String, String) {
    (
        err.code().unwrap_or_default().to_string(),
        err.message().unwrap_or_default().to_string(),
    )
}

/// UPDATE and DELETE act on the one item their key pins; any other predicate
/// is a condition on it, and RETURNING projects the old or new image.
#[tokio::test]
async fn ddb_partiql_writes_condition_and_return() {
    let server = TestServer::start().await;
    let ddb = server.dynamodb_client().await;
    create_streamed_table(&ddb, "Writes").await;
    ddb.put_item()
        .table_name("Writes")
        .item("pk", AttributeValue::S("a".into()))
        .item("name", AttributeValue::S("alpha".into()))
        .item(
            "tags",
            AttributeValue::L(vec![
                AttributeValue::S("x".into()),
                AttributeValue::S("y".into()),
                AttributeValue::S("z".into()),
            ]),
        )
        .send()
        .await
        .unwrap();

    // A false non-key predicate fails the write and leaves the item.
    let err = ddb
        .execute_statement()
        .statement("UPDATE \"Writes\" SET n = 9 WHERE pk = 'a' AND \"name\" = 'beta'")
        .send()
        .await
        .unwrap_err();
    assert_eq!(service_error(err).0, "ConditionalCheckFailedException");

    // An UPDATE of a missing item is not an upsert.
    let err = ddb
        .execute_statement()
        .statement("UPDATE \"Writes\" SET n = 9 WHERE pk = 'ghost'")
        .send()
        .await
        .unwrap_err();
    assert_eq!(service_error(err).0, "ConditionalCheckFailedException");

    // MODIFIED returns only what changed, list elements packed in index order.
    let resp = ddb
        .execute_statement()
        .statement(
            "UPDATE \"Writes\" SET tags[2] = 'c', tags[0] = 'a' REMOVE \"name\" \
             WHERE pk = 'a' RETURNING MODIFIED OLD *",
        )
        .send()
        .await
        .unwrap();
    let row = &resp.items()[0];
    let tags: Vec<&str> = row["tags"]
        .as_l()
        .unwrap()
        .iter()
        .map(|v| v.as_s().unwrap().as_str())
        .collect();
    assert_eq!(tags, ["x", "z"]);
    assert_eq!(row["name"].as_s().unwrap(), "alpha");
    assert!(!row.contains_key("pk"));

    // DELETE takes only RETURNING ALL OLD *.
    let err = ddb
        .execute_statement()
        .statement("DELETE FROM \"Writes\" WHERE pk = 'a' RETURNING ALL NEW *")
        .send()
        .await
        .unwrap_err();
    assert_eq!(
        service_error(err).1,
        "Invalid returning clause: RETURNING ALL NEW *. Only RETURNING ALL OLD * is allowed in DELETE statements."
    );
    let resp = ddb
        .execute_statement()
        .statement("DELETE FROM \"Writes\" WHERE pk = 'a' RETURNING ALL OLD *")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.items()[0]["pk"].as_s().unwrap(), "a");
    let resp = ddb
        .execute_statement()
        .statement("DELETE FROM \"Writes\" WHERE pk = 'a' RETURNING ALL OLD *")
        .send()
        .await
        .unwrap();
    assert!(resp.items().is_empty());
}

/// A read of `"table"."index"` follows the index: a GSI refuses a column it
/// does not project, and a keyed read refuses a filter on one.
#[tokio::test]
async fn ddb_partiql_index_qualified_reads_follow_the_projection() {
    use aws_sdk_dynamodb::types::{GlobalSecondaryIndex, Projection, ProjectionType};
    let server = TestServer::start().await;
    let ddb = server.dynamodb_client().await;
    ddb.create_table()
        .table_name("Indexed")
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
                        .projection_type(ProjectionType::KeysOnly)
                        .build(),
                )
                .build()
                .unwrap(),
        )
        .billing_mode(BillingMode::PayPerRequest)
        .send()
        .await
        .unwrap();
    for (pk, g) in [("a", Some("x")), ("b", Some("x")), ("c", None)] {
        let mut put = ddb
            .put_item()
            .table_name("Indexed")
            .item("pk", AttributeValue::S(pk.into()))
            .item("secret", AttributeValue::S(format!("s-{pk}")));
        if let Some(g) = g {
            put = put.item("g", AttributeValue::S(g.into()));
        }
        put.send().await.unwrap();
    }

    let pks = select_pks(&ddb, "SELECT * FROM \"Indexed\".\"by-g\" WHERE g = 'x'").await;
    assert_eq!(pks, ["a", "b"]);

    let err = ddb
        .execute_statement()
        .statement("SELECT secret FROM \"Indexed\".\"by-g\" WHERE g = 'x'")
        .send()
        .await
        .unwrap_err();
    assert_eq!(
        service_error(err).1,
        "One or more parameter values were invalid: Global secondary index by-g does not project [secret]"
    );
    let err = ddb
        .execute_statement()
        .statement("SELECT pk FROM \"Indexed\".\"by-g\" WHERE g = 'x' AND secret = 's-a'")
        .send()
        .await
        .unwrap_err();
    assert_eq!(
        service_error(err).1,
        "One or more parameter values were invalid: Secondary index by-g does not project one or more filter attributes: [secret]"
    );
    // Unkeyed, the read scans the index, which has no such attribute.
    let pks = select_pks(
        &ddb,
        "SELECT pk FROM \"Indexed\".\"by-g\" WHERE secret = 's-a'",
    )
    .await;
    assert!(pks.is_empty());

    let err = ddb
        .execute_statement()
        .statement("SELECT * FROM \"Indexed\".\"nope\"")
        .send()
        .await
        .unwrap_err();
    assert_eq!(
        service_error(err).1,
        "The table does not have the specified index"
    );
}

/// A batch member SELECT must pin the primary key, and a failed member echoes
/// its table only when it ran; a transaction refuses RETURNING up front.
#[tokio::test]
async fn ddb_partiql_batch_and_transaction_rules() {
    use aws_sdk_dynamodb::types::{
        BatchStatementErrorCodeEnum, BatchStatementRequest, ParameterizedStatement,
    };
    let server = TestServer::start().await;
    let ddb = server.dynamodb_client().await;
    create_streamed_table(&ddb, "Multi").await;
    put_row(&ddb, "Multi", "a", 1, "one").await;

    let stmt = |s: &str| {
        ParameterizedStatement::builder()
            .statement(s)
            .build()
            .unwrap()
    };
    let member = |s: &str| {
        BatchStatementRequest::builder()
            .statement(s)
            .build()
            .unwrap()
    };
    let resp = ddb
        .batch_execute_statement()
        .statements(member("SELECT * FROM \"Multi\" WHERE s = 'one'"))
        .statements(member("SELECT * FROM \"Multi\" WHERE pk = 'a'"))
        .statements(member(
            "UPDATE \"Multi\" SET n = 2 WHERE pk = 'a' AND s = 'two'",
        ))
        .send()
        .await
        .unwrap();
    let r = resp.responses();
    let err = r[0].error().unwrap();
    assert_eq!(
        err.code(),
        Some(&BatchStatementErrorCodeEnum::ValidationError)
    );
    assert!(err
        .message()
        .unwrap()
        .contains("must specify the primary key in the where clause"));
    assert!(r[0].table_name().is_none());
    assert_eq!(r[1].item().unwrap()["s"].as_s().unwrap(), "one");
    assert_eq!(
        r[2].error().unwrap().code(),
        Some(&BatchStatementErrorCodeEnum::ConditionalCheckFailed)
    );
    assert_eq!(r[2].table_name(), Some("Multi"));

    let err = ddb
        .execute_transaction()
        .transact_statements(stmt(
            "UPDATE \"Multi\" SET n = 3 WHERE pk = 'a' RETURNING ALL NEW *",
        ))
        .send()
        .await
        .unwrap_err();
    assert_eq!(
        service_error(err).1,
        "Validation failed in TransactStatements[0]: RETURNING clause is not supported in ExecuteTransaction."
    );
}
