//! dynamodb helpers `table_lookup` concerns (audit-2026-05-19).

use super::*;

/// AWS DDB ops accept either a bare table name or an ARN of the form
/// `arn:aws:dynamodb:REGION:ACCOUNT:table/NAME[/index/...|/stream/...|/backup/...]`
/// in `TableName`. Real DynamoDB normalizes both transparently; the
/// SDKs send ARNs in cross-account scenarios. Strip everything past
/// `:table/` and any sub-resource segment so callers can use one path.
pub(crate) fn resolve_table_name(input: &str) -> &str {
    if let Some(rest) = fakecloud_aws::arn::arn_resource(input, "dynamodb") {
        if let Some(after_table) = rest.split(":table/").nth(1) {
            // Drop any /index/<n>, /stream/<n>, /backup/<n> suffix.
            return after_table.split('/').next().unwrap_or(after_table);
        }
    }
    input
}

pub(crate) fn get_table<'a>(
    tables: &'a BTreeMap<String, DynamoTable>,
    name: &str,
) -> Result<&'a DynamoTable, AwsServiceError> {
    get_table_with_code(tables, name, "ResourceNotFoundException")
}

/// Variant of [`get_table`] that emits a caller-chosen wire error code.
///
/// Smithy declares different "table missing" shapes for different ops:
/// most read/write paths declare `ResourceNotFoundException`, while the
/// backup, continuous-backup, and export/restore paths declare
/// `TableNotFoundException` instead. Passing the wrong shape makes the
/// strict-mode conformance probe reject the response.
pub(crate) fn get_table_with_code<'a>(
    tables: &'a BTreeMap<String, DynamoTable>,
    name: &str,
    code: &str,
) -> Result<&'a DynamoTable, AwsServiceError> {
    let resolved = resolve_table_name(name);
    tables.get(resolved).ok_or_else(|| {
        AwsServiceError::aws_error(
            StatusCode::BAD_REQUEST,
            code,
            format!("Requested resource not found: Table: {resolved} not found"),
        )
    })
}

pub(crate) fn get_table_mut<'a>(
    tables: &'a mut BTreeMap<String, DynamoTable>,
    name: &str,
) -> Result<&'a mut DynamoTable, AwsServiceError> {
    get_table_mut_with_code(tables, name, "ResourceNotFoundException")
}

pub(crate) fn get_table_mut_with_code<'a>(
    tables: &'a mut BTreeMap<String, DynamoTable>,
    name: &str,
    code: &str,
) -> Result<&'a mut DynamoTable, AwsServiceError> {
    let resolved = resolve_table_name(name).to_string();
    tables.get_mut(&resolved).ok_or_else(|| {
        AwsServiceError::aws_error(
            StatusCode::BAD_REQUEST,
            code,
            format!("Requested resource not found: Table: {resolved} not found"),
        )
    })
}

pub(crate) fn find_table_by_arn<'a>(
    tables: &'a BTreeMap<String, DynamoTable>,
    arn: &str,
) -> Result<&'a DynamoTable, AwsServiceError> {
    tables.values().find(|t| t.arn == arn).ok_or_else(|| {
        AwsServiceError::aws_error(
            StatusCode::BAD_REQUEST,
            "ResourceNotFoundException",
            format!("Requested resource not found: {arn}"),
        )
    })
}

pub(crate) fn find_table_by_arn_mut<'a>(
    tables: &'a mut BTreeMap<String, DynamoTable>,
    arn: &str,
) -> Result<&'a mut DynamoTable, AwsServiceError> {
    tables.values_mut().find(|t| t.arn == arn).ok_or_else(|| {
        AwsServiceError::aws_error(
            StatusCode::BAD_REQUEST,
            "ResourceNotFoundException",
            format!("Requested resource not found: {arn}"),
        )
    })
}

/// The bare not-found error the item-level data plane reports.
pub(crate) fn data_table_not_found() -> AwsServiceError {
    AwsServiceError::aws_error(
        StatusCode::BAD_REQUEST,
        "ResourceNotFoundException",
        "Requested resource not found",
    )
}

/// Table lookup for the item-level data plane (GetItem, PutItem, Query, ...),
/// which reports a missing table without naming it, unlike DescribeTable.
pub(crate) fn get_data_table<'a>(
    tables: &'a BTreeMap<String, DynamoTable>,
    name: &str,
) -> Result<&'a DynamoTable, AwsServiceError> {
    tables
        .get(resolve_table_name(name))
        .ok_or_else(data_table_not_found)
}

/// Mutable variant of [`get_data_table`].
pub(crate) fn get_data_table_mut<'a>(
    tables: &'a mut BTreeMap<String, DynamoTable>,
    name: &str,
) -> Result<&'a mut DynamoTable, AwsServiceError> {
    tables
        .get_mut(resolve_table_name(name))
        .ok_or_else(data_table_not_found)
}
