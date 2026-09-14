//! Cross-account access to DynamoDB tables and streams.
//!
//! A `TableName` (or `ResourceArn`, `StreamArn`, ...) may be an ARN naming a
//! table in another account; a principal authorized by that table's
//! resource-based policy operates on it there. AWS allows this only for the
//! data plane (item operations, Query, Scan, batches, transactions), for
//! DescribeTable / UpdateTable / DeleteTable, for tagging, and for the stream
//! reads. For any other operation, and for an ARN naming another region than
//! the request's, the resource is simply not found.

use fakecloud_core::multi_account::MultiAccountState;
use fakecloud_core::service::AwsRequest;
use http::StatusCode;
use serde_json::Value;
use std::collections::BTreeMap;

use crate::state::{DynamoDbState, DynamoTable};
use fakecloud_core::service::AwsServiceError;

/// Operations that may address another account's table.
pub(crate) const CROSS_ACCOUNT_OPERATIONS: &[&str] = &[
    "GetItem",
    "PutItem",
    "UpdateItem",
    "DeleteItem",
    "Query",
    "Scan",
    "BatchGetItem",
    "BatchWriteItem",
    "TransactGetItems",
    "TransactWriteItems",
    "DescribeTable",
    "UpdateTable",
    "DeleteTable",
    "ListTagsOfResource",
    "TagResource",
    "UntagResource",
];

/// DynamoDB Streams operations that may read another account's stream.
pub(crate) const STREAMS_CROSS_ACCOUNT_OPERATIONS: &[&str] =
    &["DescribeStream", "GetShardIterator", "GetRecords"];

/// The region and account of a DynamoDB ARN (`arn:aws:dynamodb:REGION:ACCOUNT:...`).
pub(crate) fn arn_scope(arn: &str) -> Option<(&str, &str)> {
    let rest = arn.strip_prefix("arn:aws:dynamodb:")?;
    let mut parts = rest.splitn(3, ':');
    let region = parts.next()?;
    let account = parts.next()?;
    parts.next()?;
    Some((region, account))
}

/// The account that owns the table a `TableName` value names: the account in
/// a table ARN, or the caller's for a plain name.
pub(crate) fn owner_account<'a>(req: &'a AwsRequest, name_or_arn: &'a str) -> &'a str {
    match arn_scope(name_or_arn) {
        Some((_, account)) if !account.is_empty() => account,
        _ => req.account_id.as_str(),
    }
}

/// The tables of the account that owns `name_or_arn`, or none if that
/// account holds no DynamoDB state.
pub(crate) fn tables_of<'a>(
    accounts: &'a MultiAccountState<DynamoDbState>,
    req: &AwsRequest,
    name_or_arn: &str,
) -> &'a BTreeMap<String, DynamoTable> {
    static EMPTY: BTreeMap<String, DynamoTable> = BTreeMap::new();
    accounts
        .get(owner_account(req, name_or_arn))
        .map_or(&EMPTY, |state| &state.tables)
}

/// Mutable [`tables_of`]. An account that has never held DynamoDB state gets
/// an empty one, in which the table is then not found.
pub(crate) fn tables_of_mut<'a>(
    accounts: &'a mut MultiAccountState<DynamoDbState>,
    req: &AwsRequest,
    name_or_arn: &str,
) -> &'a mut BTreeMap<String, DynamoTable> {
    &mut accounts
        .get_or_create(owner_account(req, name_or_arn))
        .tables
}

/// Every table or stream ARN a request names, with the error code its
/// operation declares for a resource it cannot find.
fn referenced_arns(action: &str, body: &Value) -> Vec<(String, &'static str)> {
    let mut out = Vec::new();
    let mut push = |value: &Value, code: &'static str| {
        if let Some(s) = value.as_str() {
            if s.starts_with("arn:") {
                out.push((s.to_string(), code));
            }
        }
    };
    let table_code = match action {
        "CreateBackup"
        | "DescribeContinuousBackups"
        | "UpdateContinuousBackups"
        | "RestoreTableToPointInTime"
        | "ExportTableToPointInTime" => "TableNotFoundException",
        _ => "ResourceNotFoundException",
    };
    for field in [
        "TableName",
        "TableArn",
        "ResourceArn",
        "SourceTableArn",
        "SourceTableName",
        "StreamArn",
    ] {
        push(&body[field], table_code);
    }
    push(&body["BackupArn"], "BackupNotFoundException");
    push(&body["ExportArn"], "ExportNotFoundException");
    push(&body["ImportArn"], "ImportNotFoundException");
    if let Some(items) = body["RequestItems"].as_object() {
        for name in items.keys() {
            push(&Value::String(name.clone()), table_code);
        }
    }
    for item in body["TransactItems"].as_array().into_iter().flatten() {
        for member in ["Get", "Put", "Update", "Delete", "ConditionCheck"] {
            push(&item[member]["TableName"], table_code);
        }
    }
    if let Some(iterator) = body["ShardIterator"].as_str() {
        if let Some(stream) = iterator.split('|').next() {
            push(&Value::String(stream.to_string()), table_code);
        }
    }
    let statements = body["Statement"]
        .as_str()
        .into_iter()
        .chain(
            body["Statements"]
                .as_array()
                .into_iter()
                .chain(body["TransactStatements"].as_array())
                .flatten()
                .filter_map(|s| s["Statement"].as_str()),
        )
        .collect::<Vec<_>>();
    for statement in statements {
        if let Some((_, table)) = super::iam::partiql_verb_and_table(statement) {
            push(&Value::String(table), table_code);
        }
    }
    out
}

/// Reject a request naming a table or stream it cannot reach: one in another
/// region, or -- for an operation without cross-account support -- one in
/// another account. Both are resources that do not exist for this request.
pub(crate) fn check_references(
    req: &AwsRequest,
    body: &Value,
    cross_account_operations: &[&str],
) -> Result<(), AwsServiceError> {
    // These listings take a table only as a filter and model no not-found
    // error: a table they cannot see just matches nothing.
    if matches!(
        req.action.as_str(),
        "ListBackups" | "ListExports" | "ListImports"
    ) {
        return Ok(());
    }
    let cross_account = cross_account_operations.contains(&req.action.as_str());
    for (arn, code) in referenced_arns(&req.action, body) {
        let Some((region, account)) = arn_scope(&arn) else {
            continue;
        };
        let foreign_region = !region.is_empty() && region != req.region;
        let foreign_account = !account.is_empty() && account != req.account_id;
        if foreign_region || (foreign_account && !cross_account) {
            return Err(AwsServiceError::aws_error(
                StatusCode::BAD_REQUEST,
                code,
                format!("Requested resource not found: {arn}"),
            ));
        }
    }
    Ok(())
}

/// The account that owns the single table (or stream) an operation acts on,
/// when that is not the caller's: the request is then served in that account.
/// `None` for the caller's own resources and for batches and transactions,
/// which resolve each table's account separately.
pub(crate) fn single_resource_owner(req: &AwsRequest, body: &Value) -> Option<String> {
    let reference = match req.action.as_str() {
        "BatchGetItem" | "BatchWriteItem" | "TransactGetItems" | "TransactWriteItems" => {
            return None
        }
        "ListTagsOfResource" | "TagResource" | "UntagResource" => body["ResourceArn"].as_str(),
        "DescribeStream" | "GetShardIterator" => body["StreamArn"].as_str(),
        "GetRecords" => body["ShardIterator"]
            .as_str()
            .and_then(|it| it.split('|').next()),
        _ => body["TableName"].as_str(),
    }?;
    let (_, account) = arn_scope(reference)?;
    (!account.is_empty() && account != req.account_id).then(|| account.to_string())
}

/// A table's identity across accounts: its owner account and resolved name.
pub(crate) fn table_id(req: &AwsRequest, name_or_arn: &str) -> (String, String) {
    (
        owner_account(req, name_or_arn).to_string(),
        super::resolve_table_name(name_or_arn).to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn request(action: &str, body: Value) -> AwsRequest {
        AwsRequest {
            service: "dynamodb".into(),
            action: action.into(),
            region: "us-east-1".into(),
            account_id: "111122223333".into(),
            request_id: "r".into(),
            headers: http::HeaderMap::new(),
            query_params: std::collections::HashMap::new(),
            body: serde_json::to_vec(&body).unwrap().into(),
            body_stream: parking_lot::Mutex::new(None),
            path_segments: vec![],
            raw_path: "/".into(),
            raw_query: String::new(),
            method: http::Method::POST,
            is_query_protocol: false,
            access_key_id: None,
            principal: None,
        }
    }

    fn check(action: &str, body: Value) -> Result<(), String> {
        let req = request(action, body.clone());
        check_references(&req, &body, CROSS_ACCOUNT_OPERATIONS).map_err(|e| e.code().to_string())
    }

    const FOREIGN: &str = "arn:aws:dynamodb:us-east-1:444455556666:table/T";

    #[test]
    fn arn_scope_reads_region_and_account() {
        assert_eq!(arn_scope(FOREIGN), Some(("us-east-1", "444455556666")));
        assert_eq!(arn_scope("T"), None);
        assert_eq!(arn_scope("arn:aws:s3:::bucket"), None);
    }

    #[test]
    fn foreign_references_are_checked_per_operation() {
        assert_eq!(check("GetItem", json!({"TableName": FOREIGN})), Ok(()));
        assert_eq!(check("GetItem", json!({"TableName": "T"})), Ok(()));
        assert_eq!(
            check("UpdateTimeToLive", json!({"TableName": FOREIGN})),
            Err("ResourceNotFoundException".into())
        );
        assert_eq!(
            check("CreateBackup", json!({"TableName": FOREIGN})),
            Err("TableNotFoundException".into())
        );
        assert_eq!(
            check(
                "DescribeBackup",
                json!({"BackupArn": format!("{FOREIGN}/backup/01")})
            ),
            Err("BackupNotFoundException".into())
        );
        assert_eq!(
            check(
                "BatchExecuteStatement",
                json!({"Statements": [{"Statement": format!("SELECT * FROM \"{FOREIGN}\"")}]})
            ),
            Err("ResourceNotFoundException".into())
        );
        // A listing filtered by another account's or region's table is empty,
        // not an error.
        assert_eq!(check("ListExports", json!({"TableArn": FOREIGN})), Ok(()));
        assert_eq!(check("ListImports", json!({"TableArn": FOREIGN})), Ok(()));
        assert_eq!(check("ListBackups", json!({"TableName": FOREIGN})), Ok(()));
        let other_region = "arn:aws:dynamodb:eu-west-1:111122223333:table/T";
        for (action, body) in [
            ("GetItem", json!({"TableName": other_region})),
            (
                "BatchWriteItem",
                json!({"RequestItems": {other_region: []}}),
            ),
            (
                "TransactWriteItems",
                json!({"TransactItems": [{"ConditionCheck": {"TableName": other_region}}]}),
            ),
            ("ListTagsOfResource", json!({"ResourceArn": other_region})),
        ] {
            assert_eq!(
                check(action, body),
                Err("ResourceNotFoundException".into()),
                "{action}"
            );
        }
    }

    #[test]
    fn a_single_foreign_resource_names_its_owner() {
        let owner = |action: &str, body: Value| {
            let req = request(action, body.clone());
            single_resource_owner(&req, &body)
        };
        assert_eq!(
            owner("GetItem", json!({"TableName": FOREIGN})).as_deref(),
            Some("444455556666")
        );
        assert_eq!(owner("GetItem", json!({"TableName": "T"})), None);
        assert_eq!(
            owner(
                "GetItem",
                json!({"TableName": "arn:aws:dynamodb:us-east-1:111122223333:table/T"})
            ),
            None
        );
        assert_eq!(
            owner("TagResource", json!({"ResourceArn": FOREIGN})).as_deref(),
            Some("444455556666")
        );
        assert_eq!(
            owner(
                "GetRecords",
                json!({"ShardIterator": format!("{FOREIGN}/stream/x|shard|0")})
            )
            .as_deref(),
            Some("444455556666")
        );
        assert_eq!(
            owner("BatchGetItem", json!({"RequestItems": {FOREIGN: {}}})),
            None
        );
    }
}
