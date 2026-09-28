//! The PartiQL operations: ExecuteStatement, BatchExecuteStatement and
//! ExecuteTransaction. Parsing lives in `helpers::partiql_parse`, execution
//! in `helpers::partiql_exec`; this module owns each API's envelope -- its
//! request validation, how failures are reported, capacity, and the stream
//! and Kinesis hooks for the writes.

use std::collections::{BTreeMap, HashMap};

use http::StatusCode;
use serde_json::{json, Value};

use fakecloud_core::service::{AwsRequest, AwsResponse, AwsServiceError};
use fakecloud_core::validation::*;

use super::helpers::partiql_exec::{
    capacity_json, execute, transactional, Capacity, Change, ExecError, ExecOptions, Outcome,
    Surface,
};
use super::helpers::partiql_parse::{parse_statement, validation, Statement};
use super::{require_str_with_code, return_consumed_mode, Consumed, DynamoDbService};
use crate::state::{AttributeValue, DynamoDbState};

type Item = HashMap<String, AttributeValue>;

/// A Kinesis delivery queued until the state lock is released.
type PendingKinesis = (
    super::KinesisDeliveryTarget,
    &'static str,
    Item,
    Option<Item>,
    Option<Item>,
);

/// Record a statement's write on its table's stream, and queue the Kinesis
/// delivery for after the lock is released.
fn record_change(
    state: &mut DynamoDbState,
    table_name: &str,
    change: Change,
    pending: &mut Vec<PendingKinesis>,
) {
    let region = state.region.clone();
    let Some(table) = state.tables.get_mut(table_name) else {
        return;
    };
    if table.stream_enabled {
        if let Some(record) = crate::streams::generate_stream_record(
            table,
            change.event_name,
            change.keys.clone(),
            change.old_image.clone(),
            change.new_image.clone(),
            &region,
        ) {
            crate::streams::add_stream_record(table, record);
        }
    }
    if let Some(target) = DynamoDbService::kinesis_target(table) {
        pending.push((
            target,
            change.event_name,
            change.keys,
            change.old_image,
            change.new_image,
        ));
    }
}

/// The capacity of a batch or transaction: one entry per table, in the order
/// the tables were first touched, with its read and write units kept apart
/// so a table both read and written reports each as what it was.
#[derive(Default)]
struct CapacityByTable(Vec<(String, Consumed, Consumed)>);

impl CapacityByTable {
    fn add(&mut self, table: &str, capacity: &Capacity) {
        let index = match self.0.iter().position(|(t, ..)| t == table) {
            Some(i) => i,
            None => {
                self.0
                    .push((table.to_string(), Consumed::default(), Consumed::default()));
                self.0.len() - 1
            }
        };
        let (_, reads, writes) = &mut self.0[index];
        if capacity.read {
            reads.add(&capacity.consumed);
        } else {
            writes.add(&capacity.consumed);
        }
    }

    fn to_json(&self, mode: &str, split: bool) -> Option<Value> {
        if mode != "TOTAL" && mode != "INDEXES" {
            return None;
        }
        Some(Value::Array(
            self.0
                .iter()
                .map(|(t, r, w)| capacity_json(mode, t, r, w, split))
                .collect(),
        ))
    }
}

/// A list parameter's length bounds, reported the way AWS's request
/// validation reports them.
fn check_list_length(field: &str, list: &[Value], max: usize) -> Result<(), AwsServiceError> {
    if list.is_empty() {
        return Err(validation(format!(
            "1 validation error detected: Value '[]' at '{field}' failed to satisfy constraint: \
             Member must have length greater than or equal to 1"
        )));
    }
    if list.len() > max {
        return Err(validation(format!(
            "1 validation error detected: Value at '{field}' failed to satisfy constraint: \
             Member must have length less than or equal to {max}"
        )));
    }
    Ok(())
}

fn parameters_of(v: &Value) -> Vec<Value> {
    v["Parameters"].as_array().cloned().unwrap_or_default()
}

/// The code a failed batch or transaction member reports.
fn member_code(error: &AwsServiceError) -> &'static str {
    match error.code() {
        "ConditionalCheckFailedException" => "ConditionalCheckFailed",
        "ResourceNotFoundException" => "ResourceNotFound",
        "DuplicateItemException" => "DuplicateItem",
        "TransactionConflictException" => "TransactionConflict",
        "ProvisionedThroughputExceededException" => "ProvisionedThroughputExceeded",
        "ThrottlingException" => "ThrottlingError",
        _ => "ValidationError",
    }
}

impl DynamoDbService {
    pub(super) fn execute_statement(
        &self,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        let body = Self::parse_body(req)?;
        let statement = require_str_with_code(&body, "Statement", "ValidationException")?;
        validate_optional_enum_value(
            "returnConsumedCapacity",
            &body["ReturnConsumedCapacity"],
            &["INDEXES", "TOTAL", "NONE"],
        )?;
        validate_optional_enum_value(
            "returnValuesOnConditionCheckFailure",
            &body["ReturnValuesOnConditionCheckFailure"],
            &["ALL_OLD", "NONE"],
        )?;
        // AWS requires Limit >= 1 when present; a `Limit: 0` (or negative) is a
        // ValidationException, not "no limit".
        let limit = match body["Limit"].as_i64() {
            Some(n) if n < 1 => {
                return Err(validation(format!(
                    "1 validation error detected: Value '{n}' at 'limit' failed to satisfy \
                     constraint: Member must have value greater than or equal to 1"
                )));
            }
            other => other.map(|n| n as usize),
        };
        let parameters = parameters_of(&body);
        // Everything that is a property of the statement alone answers before
        // any table is looked up.
        let stmt = parse_statement(statement, &parameters)?;
        let opts = ExecOptions {
            consistent_read: body["ConsistentRead"].as_bool().unwrap_or(false),
            limit,
            next_token: body["NextToken"].as_str(),
            return_old_on_condition_failure: body["ReturnValuesOnConditionCheckFailure"].as_str()
                == Some("ALL_OLD"),
            ..ExecOptions::new(Surface::Execute)
        };

        let mut pending = Vec::new();
        let response = {
            let mut accounts = self.state.write();
            let state = accounts.get_or_create(&req.account_id);
            let outcome = execute(&mut state.tables, &stmt, &opts).map_err(|e| *e.error)?;
            let mut response = json!({});
            if outcome.returns_items {
                response["Items"] = json!(outcome.items);
            }
            if let Some(token) = &outcome.next_token {
                response["NextToken"] = json!(token);
            }
            let cc =
                outcome
                    .capacity
                    .to_json(return_consumed_mode(&body), &outcome.table_name, false);
            if !cc.is_null() {
                response["ConsumedCapacity"] = cc;
            }
            if let Some(change) = outcome.change {
                record_change(state, &outcome.table_name, change, &mut pending);
            }
            response
        };
        self.deliver_pending(pending);
        Self::ok_json(response)
    }

    pub(super) fn batch_execute_statement(
        &self,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        let body = Self::parse_body(req)?;
        validate_optional_enum_value(
            "returnConsumedCapacity",
            &body["ReturnConsumedCapacity"],
            &["INDEXES", "TOTAL", "NONE"],
        )?;
        let statements = body["Statements"]
            .as_array()
            .ok_or_else(|| validation("Statements is required"))?;
        check_list_length("statements", statements, 25)?;

        let mut pending = Vec::new();
        let mut capacity = CapacityByTable::default();
        let responses: Vec<Value> = {
            let mut accounts = self.state.write();
            let state = accounts.get_or_create(&req.account_id);
            statements
                .iter()
                .map(|member| {
                    let statement = member["Statement"].as_str().unwrap_or_default();
                    let opts = ExecOptions {
                        consistent_read: member["ConsistentRead"].as_bool().unwrap_or(false),
                        ..ExecOptions::new(Surface::Batch)
                    };
                    let result = parse_statement(statement, &parameters_of(member))
                        .map_err(ExecError::from)
                        .and_then(|stmt| execute(&mut state.tables, &stmt, &opts));
                    match result {
                        Ok(outcome) => {
                            capacity.add(&outcome.table_name, &outcome.capacity);
                            let mut response = json!({ "TableName": outcome.table_name });
                            // The singular Item cannot hold an empty row, so a
                            // statement returning none omits it.
                            if let Some(item) = outcome.items.first() {
                                response["Item"] = json!(item);
                            }
                            if let Some(change) = outcome.change {
                                record_change(state, &outcome.table_name, change, &mut pending);
                            }
                            response
                        }
                        Err(ExecError { error, table }) => {
                            let mut response = json!({
                                "Error": {
                                    "Code": member_code(&error),
                                    "Message": error.message(),
                                }
                            });
                            // Echoed only by a statement that reached its table.
                            if let Some(table) = table {
                                response["TableName"] = json!(table);
                            }
                            response
                        }
                    }
                })
                .collect()
        };
        self.deliver_pending(pending);
        let mut result = json!({ "Responses": responses });
        if let Some(cc) = capacity.to_json(return_consumed_mode(&body), false) {
            result["ConsumedCapacity"] = cc;
        }
        Self::ok_json(result)
    }

    pub(super) fn execute_transaction(
        &self,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        let body = Self::parse_body(req)?;
        validate_optional_string_length(
            "clientRequestToken",
            body["ClientRequestToken"].as_str(),
            1,
            36,
        )?;
        validate_optional_enum_value(
            "returnConsumedCapacity",
            &body["ReturnConsumedCapacity"],
            &["INDEXES", "TOTAL", "NONE"],
        )?;
        let members = body["TransactStatements"]
            .as_array()
            .ok_or_else(|| validation("TransactStatements is required"))?;
        check_list_length("transactStatements", members, 100)?;

        // Every statement is validated before any runs: a statement that can
        // never run in a transaction fails the whole request up front rather
        // than cancelling it.
        let mut statements: Vec<Statement> = Vec::with_capacity(members.len());
        for (i, member) in members.iter().enumerate() {
            let in_member = |e: AwsServiceError| {
                validation(format!(
                    "Validation failed in TransactStatements[{i}]: {}",
                    e.message()
                ))
            };
            let stmt = parse_statement(
                member["Statement"].as_str().unwrap_or_default(),
                &parameters_of(member),
            )
            .map_err(in_member)?;
            if stmt.returning().is_some() {
                return Err(in_member(validation(
                    "RETURNING clause is not supported in ExecuteTransaction.",
                )));
            }
            if stmt.is_read() && stmt.index().is_some() {
                return Err(in_member(validation(
                    "Reads on indices are not supported within transactions.",
                )));
            }
            statements.push(stmt);
        }

        // Idempotency: a retried ExecuteTransaction carrying the same
        // ClientRequestToken (within the window) is applied at most once. The
        // replay reports a transactional read of the stored result in place of
        // the write.
        let client_token = body["ClientRequestToken"].as_str().map(str::to_string);
        let request_hash = {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            req.body.hash(&mut h);
            h.finish()
        };

        let mut accounts = self.state.write();
        if let Some(token) = client_token.as_deref() {
            if let Some(cached) =
                self.transact_idempotency_lookup(&req.account_id, token, request_hash)?
            {
                let mut replay: Value =
                    serde_json::from_slice(cached.body.expect_bytes()).unwrap_or_default();
                replay_as_read(&mut replay);
                return Self::ok_json(replay);
            }
        }
        let state = accounts.get_or_create(&req.account_id);

        // Run every statement against copies of the tables it touches; only a
        // transaction whose every statement succeeded is committed.
        let mut scratch: BTreeMap<String, crate::state::DynamoTable> = BTreeMap::new();
        for stmt in &statements {
            let table = super::get_table(&state.tables, stmt.table())?;
            scratch
                .entry(table.name.clone())
                .or_insert_with(|| table.clone());
        }
        let opts = ExecOptions::new(Surface::Transaction);
        let results: Vec<Result<Outcome, ExecError>> = statements
            .iter()
            .map(|stmt| execute(&mut scratch, stmt, &opts))
            .collect();
        if results.iter().any(Result::is_err) {
            let reasons: Vec<Value> = results
                .iter()
                .map(|r| match r {
                    Ok(_) => json!({ "Code": "None" }),
                    Err(ExecError { error, .. }) => json!({
                        "Code": member_code(error),
                        "Message": error.message(),
                    }),
                })
                .collect();
            let codes: Vec<&str> = reasons.iter().filter_map(|r| r["Code"].as_str()).collect();
            let error_body = json!({
                "__type": "TransactionCanceledException",
                "message": format!(
                    "Transaction cancelled, please refer cancellation reasons for specific reasons [{}]",
                    codes.join(", ")
                ),
                "CancellationReasons": reasons,
            });
            return Ok(AwsResponse::json(
                StatusCode::BAD_REQUEST,
                serde_json::to_vec(&error_body).unwrap_or_default(),
            ));
        }

        for (name, table) in scratch {
            state.tables.insert(name, table);
        }
        let mut pending = Vec::new();
        let mut capacity = CapacityByTable::default();
        let mut responses = Vec::with_capacity(results.len());
        for outcome in results.into_iter().flatten() {
            capacity.add(
                &outcome.table_name,
                &transactional(outcome.capacity.clone()),
            );
            let mut response = json!({});
            if let Some(item) = outcome.items.first() {
                response["Item"] = json!(item);
            }
            responses.push(response);
            if let Some(change) = outcome.change {
                record_change(state, &outcome.table_name, change, &mut pending);
            }
        }
        let mut result = json!({ "Responses": responses });
        if let Some(cc) = capacity.to_json(return_consumed_mode(&body), true) {
            result["ConsumedCapacity"] = cc;
        }
        // Stored under the write lock, atomically with the apply, so an
        // identical retry replays this result rather than re-applying.
        if let Some(token) = client_token.as_deref() {
            self.transact_idempotency_store(&req.account_id, token, request_hash, &result);
        }
        drop(accounts);
        self.deliver_pending(pending);
        Self::ok_json(result)
    }

    fn deliver_pending(&self, pending: Vec<PendingKinesis>) {
        for (target, event_name, keys, old_image, new_image) in pending {
            self.deliver_to_kinesis_destinations(
                &target,
                event_name,
                &keys,
                old_image.as_ref(),
                new_image.as_ref(),
            );
        }
    }
}

/// A replayed transaction reports reading the stored result: every write
/// unit of the original becomes a read unit.
fn replay_as_read(result: &mut Value) {
    fn swap(arm: &mut Value) {
        if let Some(obj) = arm.as_object_mut() {
            if let Some(units) = obj.remove("WriteCapacityUnits") {
                let read = obj
                    .get("ReadCapacityUnits")
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0);
                obj.insert(
                    "ReadCapacityUnits".to_string(),
                    json!(read + units.as_f64().unwrap_or(0.0)),
                );
            }
        }
    }
    for entry in result["ConsumedCapacity"]
        .as_array_mut()
        .into_iter()
        .flatten()
    {
        swap(entry);
        if let Some(table) = entry.get_mut("Table") {
            swap(table);
        }
        for group in ["GlobalSecondaryIndexes", "LocalSecondaryIndexes"] {
            if let Some(map) = entry.get_mut(group).and_then(Value::as_object_mut) {
                map.values_mut().for_each(swap);
            }
        }
    }
}
