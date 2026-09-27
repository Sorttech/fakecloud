//! DynamoDB-specific IAM condition keys, for fine-grained access control.
//!
//! - `dynamodb:LeadingKeys` (alias `dynamodb:FirstPartitionKeyValues`): the
//!   partition-key values of the items a request addresses -- a single
//!   item's key, the partition-key equality of a Query, every key or item a
//!   batch or transaction sends to the table being authorized, a PartiQL
//!   statement's partition-key equality or inserted item. Absent for a Scan,
//!   which addresses no particular partition.
//! - `dynamodb:Attributes`: the top-level attribute names the request
//!   specifies -- key and item attributes, projections, `AttributesToGet`,
//!   and every attribute an update, condition, filter or key-condition
//!   expression references. A request with no projection reads every
//!   attribute yet names only its key attributes, which is why AWS pairs
//!   this key with `dynamodb:Select`.
//! - `dynamodb:Select`: the `Select` parameter, or the value DynamoDB
//!   applies without one: `SPECIFIC_ATTRIBUTES` with a projection,
//!   `ALL_PROJECTED_ATTRIBUTES` for an index query, `ALL_ATTRIBUTES`
//!   otherwise. Only on operations that return item attributes.
//! - `dynamodb:ReturnValues`: the `ReturnValues` parameter, `NONE` by default
//!   on a single-item write.
//! - `dynamodb:ReturnConsumedCapacity`: the parameter, `NONE` by default.
//! - `dynamodb:EnclosingOperation`: the transaction an item action runs in
//!   (`TransactWriteItems`, `TransactGetItems`, `ExecuteTransaction`).
//! - `dynamodb:FullTableScan`: whether a PartiQL SELECT lacks a
//!   partition-key equality and so reads the whole table.

use std::collections::{BTreeMap, BTreeSet};

use fakecloud_core::auth::IamAction;
use fakecloud_core::service::AwsRequest;
use serde_json::Value;

use crate::state::{attribute_type_and_value, SharedDynamoDbState};

/// Words in DynamoDB and PartiQL expressions that are never attribute names.
/// Operator words of both DynamoDB expressions and PartiQL: never attribute
/// names. Anything else is reported -- an extra name only narrows what an
/// attribute allow-list admits, a missing one would widen it.
const EXPRESSION_WORDS: &[&str] = &["and", "or", "not", "between", "in"];

/// UpdateExpression clause keywords. DynamoDB reserves them, so an attribute
/// named `set` is always written `#name` in a native expression.
const UPDATE_CLAUSE_WORDS: &[&str] = &["set", "remove", "add", "delete"];

struct Keys {
    /// `None` when the partition keys could not be determined: the key is
    /// then omitted, so a set operator cannot treat it as an empty match.
    /// `Some(empty)` when the request addresses no particular partition.
    leading: Option<BTreeSet<String>>,
    attributes: BTreeSet<String>,
    select: Option<String>,
    return_values: Option<String>,
    return_consumed_capacity: Option<String>,
    enclosing_operation: Option<&'static str>,
    full_table_scan: Option<bool>,
}

impl Default for Keys {
    fn default() -> Self {
        Self {
            leading: Some(BTreeSet::new()),
            attributes: BTreeSet::new(),
            select: None,
            return_values: None,
            return_consumed_capacity: None,
            enclosing_operation: None,
            full_table_scan: None,
        }
    }
}

impl Keys {
    /// Record a statement's or member's `Select`. Across the statements of a
    /// batch or the members of a transaction the request reads as much as
    /// its most permissive one, so that is the value reported.
    fn merge_select(&mut self, select: String) {
        fn rank(select: &str) -> u8 {
            match select {
                "COUNT" => 0,
                "SPECIFIC_ATTRIBUTES" => 1,
                "ALL_PROJECTED_ATTRIBUTES" => 2,
                _ => 3,
            }
        }
        if self
            .select
            .as_deref()
            .is_none_or(|current| rank(&select) > rank(current))
        {
            self.select = Some(select);
        }
    }

    fn add_leading(&mut self, value: String) {
        if let Some(set) = &mut self.leading {
            set.insert(value);
        }
    }

    fn into_map(self) -> BTreeMap<String, Vec<String>> {
        let mut out = BTreeMap::new();
        // An empty list means "no values" to set operators (ForAllValues is
        // vacuously true); an omitted key means "unknown".
        if let Some(leading) = self.leading {
            let leading: Vec<String> = leading.into_iter().collect();
            out.insert(
                "dynamodb:firstpartitionkeyvalues".to_string(),
                leading.clone(),
            );
            out.insert("dynamodb:leadingkeys".to_string(), leading);
        }
        out.insert(
            "dynamodb:attributes".to_string(),
            self.attributes.into_iter().collect(),
        );
        for (key, value) in [
            ("dynamodb:select", self.select),
            ("dynamodb:returnvalues", self.return_values),
            (
                "dynamodb:returnconsumedcapacity",
                self.return_consumed_capacity,
            ),
            (
                "dynamodb:enclosingoperation",
                self.enclosing_operation.map(str::to_string),
            ),
            (
                "dynamodb:fulltablescan",
                self.full_table_scan.map(|b| b.to_string()),
            ),
        ] {
            if let Some(v) = value {
                out.insert(key.to_string(), vec![v]);
            }
        }
        out
    }
}

/// The table (and index, if any) a resource ARN names, with the partition
/// key attribute the ARN's key conditions are about.
struct Target {
    account: String,
    table_ref_name: String,
    partition_key: String,
    index: Option<String>,
}

fn target(
    accounts: &fakecloud_core::multi_account::MultiAccountState<crate::state::DynamoDbState>,
    resource: &str,
) -> Option<Target> {
    let rest = fakecloud_aws::arn::arn_resource(resource, "dynamodb")?;
    let (scope, path) = rest.split_once(":table/")?;
    let account = scope.split(':').nth(1)?;
    let mut segments = path.split('/');
    let name = segments.next()?;
    let index = match (segments.next(), segments.next()) {
        (Some("index"), Some(index)) => Some(index.to_string()),
        _ => None,
    };
    let table = accounts.get(account)?.tables.get(name)?;
    let partition_key = match &index {
        Some(index) => table
            .gsi
            .iter()
            .map(|g| (&g.index_name, &g.key_schema))
            .chain(table.lsi.iter().map(|l| (&l.index_name, &l.key_schema)))
            .find(|(n, _)| *n == index)
            .and_then(|(_, ks)| ks.iter().find(|k| k.key_type == "HASH"))
            .map(|k| k.attribute_name.clone())
            .unwrap_or_else(|| table.hash_key_name().to_string()),
        None => table.hash_key_name().to_string(),
    };
    Some(Target {
        account: account.to_string(),
        table_ref_name: name.to_string(),
        partition_key,
        index,
    })
}

/// Whether a request's `TableName` value names `target`'s table: the same
/// name in the same account. A batch or transaction may name same-named
/// tables in several accounts (one by name, others by ARN), and each table's
/// authorization sees only the keys and attributes sent to that table.
fn names_table(target: &Target, caller_account: &str, table_name: Option<&str>) -> bool {
    let Some(name) = table_name else {
        return false;
    };
    let owner = match super::cross_account::arn_scope(name) {
        Some((_, account)) if !account.is_empty() => account,
        _ => caller_account,
    };
    owner == target.account && super::resolve_table_name(name) == target.table_ref_name
}

/// The string an IAM condition compares for a scalar attribute value: the
/// string, the number's digits, or the binary's base64.
fn scalar_string(v: &Value) -> Option<String> {
    match attribute_type_and_value(v)? {
        // Numbers compare by value in DynamoDB (`1.0` is key `1`), so the key
        // is reported in canonical form.
        ("N", Value::String(n)) => {
            Some(super::helpers::partiql::canonical_number(n).unwrap_or_else(|| n.clone()))
        }
        ("S" | "B", Value::String(s)) => Some(s.clone()),
        ("BOOL", Value::Bool(b)) => Some(b.to_string()),
        _ => None,
    }
}

/// The DynamoDB condition keys for one authorization of `request`.
pub(crate) fn condition_keys(
    state: &SharedDynamoDbState,
    request: &AwsRequest,
    action: &IamAction,
) -> BTreeMap<String, Vec<String>> {
    let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
    let accounts = state.read();
    let target = target(&accounts, &action.resource);
    // The account a plain table name resolves in, as `iam::actions_for`
    // resolves it.
    let caller_account = request
        .principal
        .as_ref()
        .map(|p| p.account_id.as_str())
        .unwrap_or(request.account_id.as_str());
    let mut keys = Keys::default();
    let rcc = || {
        Some(
            body["ReturnConsumedCapacity"]
                .as_str()
                .unwrap_or("NONE")
                .to_string(),
        )
    };
    let names = expression_names(&body);

    match request.action.as_str() {
        "GetItem" | "PutItem" | "UpdateItem" | "DeleteItem" => {
            let item = if request.action == "PutItem" {
                &body["Item"]
            } else {
                &body["Key"]
            };
            if let Some(t) = &target {
                add_leading(&mut keys, item, &t.partition_key);
            }
            add_item_attributes(&mut keys, item);
            add_request_attributes(&mut keys, &body, &names);
            keys.return_consumed_capacity = rcc();
            if request.action == "GetItem" {
                keys.merge_select(implicit_select(&body, false));
            } else {
                keys.return_values =
                    Some(body["ReturnValues"].as_str().unwrap_or("NONE").to_string());
            }
        }
        "Query" => {
            if let Some(t) = &target {
                match query_partition_value(&body, &names, &t.partition_key) {
                    Some(v) => keys.add_leading(v),
                    None => keys.leading = None,
                }
                keys.merge_select(implicit_select(&body, t.index.is_some()));
            }
            add_request_attributes(&mut keys, &body, &names);
            keys.return_consumed_capacity = rcc();
        }
        "Scan" => {
            add_request_attributes(&mut keys, &body, &names);
            keys.merge_select(implicit_select(&body, body["IndexName"].is_string()));
            keys.return_consumed_capacity = rcc();
        }
        "BatchGetItem" | "BatchWriteItem" => {
            if let (Some(t), Some(items)) = (&target, body["RequestItems"].as_object()) {
                for (table_name, entry) in items {
                    if !names_table(t, caller_account, Some(table_name)) {
                        continue;
                    }
                    if request.action == "BatchGetItem" {
                        let entry_names = expression_names(entry);
                        for key in entry["Keys"].as_array().into_iter().flatten() {
                            add_leading(&mut keys, key, &t.partition_key);
                            add_item_attributes(&mut keys, key);
                        }
                        add_request_attributes(&mut keys, entry, &entry_names);
                        keys.merge_select(implicit_select(entry, false));
                    } else {
                        for write in entry.as_array().into_iter().flatten() {
                            let item = if write["PutRequest"].is_object() {
                                &write["PutRequest"]["Item"]
                            } else {
                                &write["DeleteRequest"]["Key"]
                            };
                            add_leading(&mut keys, item, &t.partition_key);
                            add_item_attributes(&mut keys, item);
                        }
                    }
                }
            }
            keys.return_consumed_capacity = rcc();
        }
        "TransactGetItems" | "TransactWriteItems" => {
            keys.enclosing_operation = Some(if request.action == "TransactGetItems" {
                "TransactGetItems"
            } else {
                "TransactWriteItems"
            });
            let member = match action.action {
                "GetItem" => "Get",
                "PutItem" => "Put",
                "UpdateItem" => "Update",
                "DeleteItem" => "Delete",
                _ => "ConditionCheck",
            };
            if let Some(t) = &target {
                for item in body["TransactItems"].as_array().into_iter().flatten() {
                    let op = &item[member];
                    if !names_table(t, caller_account, op["TableName"].as_str()) {
                        continue;
                    }
                    let addressed = if member == "Put" {
                        &op["Item"]
                    } else {
                        &op["Key"]
                    };
                    add_leading(&mut keys, addressed, &t.partition_key);
                    add_item_attributes(&mut keys, addressed);
                    add_request_attributes(&mut keys, op, &expression_names(op));
                    if member == "Get" {
                        keys.merge_select(implicit_select(op, false));
                    }
                }
            }
            keys.return_consumed_capacity = rcc();
        }
        "ExecuteStatement" | "BatchExecuteStatement" | "ExecuteTransaction" => {
            if request.action == "ExecuteTransaction" {
                keys.enclosing_operation = Some("ExecuteTransaction");
            }
            let statements: Vec<(&str, &[Value])> = match request.action.as_str() {
                "ExecuteStatement" => vec![(
                    body["Statement"].as_str().unwrap_or(""),
                    body["Parameters"].as_array().map_or(&[][..], Vec::as_slice),
                )],
                _ => {
                    let list = if request.action == "BatchExecuteStatement" {
                        &body["Statements"]
                    } else {
                        &body["TransactStatements"]
                    };
                    list.as_array()
                        .into_iter()
                        .flatten()
                        .map(|s| {
                            (
                                s["Statement"].as_str().unwrap_or(""),
                                s["Parameters"].as_array().map_or(&[][..], Vec::as_slice),
                            )
                        })
                        .collect()
                }
            };
            if let Some(t) = &target {
                for (statement, parameters) in statements {
                    let mapped = super::iam::partiql_verb_and_table(statement);
                    match mapped {
                        Some((verb, table_name)) if verb == action.action => {
                            if super::resolve_table_name(&table_name) != t.table_ref_name {
                                continue;
                            }
                            add_partiql_keys(&mut keys, t, statement, parameters, verb);
                        }
                        _ => {}
                    }
                }
            }
            if request.action == "ExecuteStatement" {
                keys.return_consumed_capacity = rcc();
            }
        }
        _ => {}
    }
    keys.into_map()
}

fn add_leading(keys: &mut Keys, item: &Value, partition_key: &str) {
    match item.get(partition_key).and_then(scalar_string) {
        Some(v) => keys.add_leading(v),
        // An item without its partition key is rejected by the handler, but
        // it is never a known empty set.
        None => keys.leading = None,
    }
}

fn add_item_attributes(keys: &mut Keys, item: &Value) {
    if let Some(obj) = item.as_object() {
        keys.attributes.extend(obj.keys().cloned());
    }
}

fn expression_names(body: &Value) -> BTreeMap<String, String> {
    body["ExpressionAttributeNames"]
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
        .collect()
}

/// Attributes named by a request's projections, expressions and legacy
/// condition parameters.
fn add_request_attributes(keys: &mut Keys, body: &Value, names: &BTreeMap<String, String>) {
    if let Some(update) = body["UpdateExpression"].as_str() {
        keys.attributes.extend(update_targets(update, names));
    }
    for expr in [
        "ProjectionExpression",
        "UpdateExpression",
        "ConditionExpression",
        "FilterExpression",
        "KeyConditionExpression",
    ] {
        if let Some(text) = body[expr].as_str() {
            keys.attributes.extend(expression_attributes(text, names));
        }
    }
    for list in ["AttributesToGet"] {
        for name in body[list].as_array().into_iter().flatten() {
            if let Some(n) = name.as_str() {
                keys.attributes.insert(n.to_string());
            }
        }
    }
    for map in [
        "AttributeUpdates",
        "Expected",
        "KeyConditions",
        "QueryFilter",
        "ScanFilter",
    ] {
        if let Some(obj) = body[map].as_object() {
            keys.attributes.extend(obj.keys().cloned());
        }
    }
}

/// The attributes an update expression may write. Each target is reported
/// both as its first path segment (`SET a.b = ...` writes inside `a`) and as
/// its whole text with names resolved: depending on its shape the executor
/// can also write a top-level attribute literally named `a.b[0]`, or split a
/// quoted `"a.b"` into a path. Reporting both can only narrow what an
/// attribute allow-list admits.
fn update_targets(expr: &str, names: &BTreeMap<String, String>) -> Vec<String> {
    use super::helpers::{parse_update_clauses, UpdateAction};
    let resolve = |segment: &str| {
        let segment = segment.trim().trim_matches('"');
        names
            .get(segment)
            .cloned()
            .unwrap_or_else(|| segment.to_string())
    };
    let mut out = Vec::new();
    for (action, assignments) in parse_update_clauses(expr) {
        for assignment in &assignments {
            let target = match action {
                UpdateAction::Set => match assignment.split_once('=') {
                    Some((left, _)) => left,
                    None => continue,
                },
                UpdateAction::Remove => assignment.as_str(),
                UpdateAction::Add | UpdateAction::Delete => {
                    assignment.split_whitespace().next().unwrap_or_default()
                }
            }
            .trim();
            if target.is_empty() {
                continue;
            }
            let unquoted = target.trim_matches('"');
            let first = unquoted.split(['.', '[']).next().unwrap_or(unquoted);
            out.push(resolve(first));
            let whole = unquoted
                .split('.')
                .map(|segment| match segment.split_once('[') {
                    Some((name, index)) => format!("{}[{index}", resolve(name)),
                    None => resolve(segment),
                })
                .collect::<Vec<_>>()
                .join(".");
            out.push(whole);
        }
    }
    out
}

fn implicit_select(body: &Value, index: bool) -> String {
    if let Some(select) = body["Select"].as_str() {
        return select.to_string();
    }
    if body["ProjectionExpression"].is_string() || body["AttributesToGet"].is_array() {
        "SPECIFIC_ATTRIBUTES".to_string()
    } else if index {
        "ALL_PROJECTED_ATTRIBUTES".to_string()
    } else {
        "ALL_ATTRIBUTES".to_string()
    }
}

/// A token of a DynamoDB or PartiQL expression.
#[derive(Debug, PartialEq)]
enum Token {
    /// An attribute reference or keyword, with whether it directly follows a
    /// `.` (a nested path segment) and whether it was double-quoted.
    Name {
        text: String,
        nested: bool,
        quoted: bool,
    },
    /// A `:placeholder`, a `?` parameter, a string or number literal.
    Value,
    Symbol(char),
}

fn tokenize(text: &str) -> Vec<Token> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let nested = matches!(out.last(), Some(Token::Symbol('.')));
        if c.is_whitespace() {
            i += 1;
        } else if c == '\'' {
            // A PartiQL string literal; '' escapes a quote.
            i += 1;
            while i < chars.len() {
                if chars[i] == '\'' {
                    if chars.get(i + 1) == Some(&'\'') {
                        i += 2;
                        continue;
                    }
                    break;
                }
                i += 1;
            }
            i += 1;
            out.push(Token::Value);
        } else if c == '"' {
            let start = i + 1;
            i = start;
            while i < chars.len() && chars[i] != '"' {
                i += 1;
            }
            out.push(Token::Name {
                text: chars[start..i.min(chars.len())].iter().collect(),
                nested,
                quoted: true,
            });
            i += 1;
        } else if c == ':' || c.is_ascii_digit() || c == '?' {
            i += 1;
            while i < chars.len() && (chars[i].is_alphanumeric() || matches!(chars[i], '_' | '.')) {
                if chars[i] == '.' && c != ':' && !chars[i - 1].is_ascii_digit() {
                    break;
                }
                i += 1;
            }
            out.push(Token::Value);
        } else if c.is_alphabetic() || c == '_' || c == '#' {
            let start = i;
            i += 1;
            while i < chars.len() && (chars[i].is_alphanumeric() || matches!(chars[i], '_' | '-')) {
                i += 1;
            }
            out.push(Token::Name {
                text: chars[start..i].iter().collect(),
                nested,
                quoted: false,
            });
        } else {
            out.push(Token::Symbol(c));
            i += 1;
        }
    }
    out
}

/// Top-level attribute names an expression references: every name that is
/// not a keyword, a function call, a nested path segment or a list index.
fn expression_attributes(text: &str, names: &BTreeMap<String, String>) -> Vec<String> {
    let tokens = tokenize(text);
    let mut out = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        let Token::Name {
            text,
            nested,
            quoted,
        } = &tokens[i]
        else {
            i += 1;
            continue;
        };
        let lower = text.to_ascii_lowercase();
        let keyword = !quoted
            && (EXPRESSION_WORDS.contains(&lower.as_str())
                || UPDATE_CLAUSE_WORDS.contains(&lower.as_str()));
        let function = matches!(tokens.get(i + 1), Some(Token::Symbol('(')));
        if !nested && !keyword && !function {
            if let Some(name) = text.strip_prefix('#').map(|_| names.get(text)) {
                if let Some(resolved) = name {
                    out.push(resolved.clone());
                }
            } else {
                out.push(text.clone());
            }
        }
        i += 1;
    }
    out
}

/// The value a Query's key condition fixes the partition key to, found the
/// way the handler evaluates the condition: split on top-level AND, strip
/// one layer of enclosing parentheses, recurse.
fn query_partition_value(
    body: &Value,
    names: &BTreeMap<String, String>,
    partition_key: &str,
) -> Option<String> {
    if let Some(cond) = body["KeyConditions"][partition_key].as_object() {
        return cond
            .get("AttributeValueList")?
            .as_array()?
            .first()
            .and_then(scalar_string);
    }
    let text = body["KeyConditionExpression"].as_str()?;
    key_condition_partition_value(
        text,
        names,
        &body["ExpressionAttributeValues"],
        partition_key,
    )
}

fn key_condition_partition_value(
    expr: &str,
    names: &BTreeMap<String, String>,
    values: &Value,
    partition_key: &str,
) -> Option<String> {
    use super::helpers::{split_on_and, strip_outer_parens};
    let trimmed = expr.trim();
    let parts = split_on_and(trimmed);
    if parts.len() > 1 {
        return parts
            .iter()
            .find_map(|part| key_condition_partition_value(part, names, values, partition_key));
    }
    let stripped = strip_outer_parens(trimmed);
    if stripped != trimmed {
        return key_condition_partition_value(stripped, names, values, partition_key);
    }
    if trimmed.to_ascii_lowercase().starts_with("begins_with") {
        return None;
    }
    let (op, pos) = ["<=", ">=", "<>", "=", "<", ">"]
        .iter()
        .find_map(|cand| trimmed.find(cand).map(|pos| (*cand, pos)))?;
    if op != "=" {
        return None;
    }
    let left = trimmed[..pos].trim().trim_matches('"');
    let right = trimmed[pos + 1..].trim();
    if !right.starts_with(':') || right.contains(char::is_whitespace) {
        return None;
    }
    let attr = if left.starts_with('#') {
        names.get(left).map(String::as_str)
    } else {
        Some(left)
    };
    if attr == Some(partition_key) {
        return values.get(right).and_then(scalar_string);
    }
    None
}

/// The PartiQL condition keys for one statement, read from the same parse
/// the executor runs -- the same grammar and the same `?` parameter binding
/// -- so a statement the executor accepts cannot be read here as touching
/// different partitions or attributes than it does. A statement that does not
/// parse is rejected by the executor, so it has no keys to report.
fn add_partiql_keys(
    keys: &mut Keys,
    target: &Target,
    statement: &str,
    parameters: &[Value],
    verb: &str,
) {
    use super::helpers::partiql_exec::insert_item;
    use super::helpers::partiql_parse::{
        conjuncts, expr_attributes, key_equality, key_values, parse_statement, path_root,
        pinned_values, Projection, Statement, UpdateOp,
    };

    let Ok(stmt) = parse_statement(statement, parameters) else {
        return;
    };
    let stmt = match stmt {
        Statement::Exists(inner) => *inner,
        other => other,
    };
    let mut attrs = Vec::new();
    let filter = match &stmt {
        Statement::Insert { value, .. } => {
            // The item exactly as the executor builds it, bound parameters
            // included; one it cannot build is rejected there.
            match insert_item(value) {
                Ok(item) => {
                    match item.get(&target.partition_key).and_then(scalar_string) {
                        Some(v) => keys.add_leading(v),
                        None => keys.leading = None,
                    }
                    keys.attributes.extend(item.into_keys());
                }
                Err(_) => keys.leading = None,
            }
            return;
        }
        Statement::Select {
            projection, filter, ..
        } => {
            match projection {
                Projection::Paths(paths) => {
                    attrs.extend(paths.iter().map(|p| path_root(p).to_string()));
                    keys.merge_select("SPECIFIC_ATTRIBUTES".to_string());
                }
                Projection::Star => keys.merge_select(if target.index.is_some() {
                    "ALL_PROJECTED_ATTRIBUTES".to_string()
                } else {
                    "ALL_ATTRIBUTES".to_string()
                }),
            }
            filter.as_ref()
        }
        Statement::Update { ops, filter, .. } => {
            for op in ops {
                match op {
                    UpdateOp::Set(path, value) => {
                        attrs.push(path_root(path).to_string());
                        // Attributes the assignment reads; an extra name only
                        // narrows what an attribute allow-list admits.
                        expr_attributes(value, &mut attrs);
                    }
                    UpdateOp::Remove(path) => attrs.push(path_root(path).to_string()),
                }
            }
            filter.as_ref()
        }
        Statement::Delete { filter, .. } => filter.as_ref(),
        Statement::Exists(_) => return,
    };
    if let Some(expr) = filter {
        expr_attributes(expr, &mut attrs);
    }
    keys.attributes.extend(attrs);
    // The partition the executor targets: a write's key comes from the first
    // key equality (the same function the executor pins its item with), a
    // read's from its first equality or IN conjunct; only a read with neither
    // falls back to every partition its OR branches can reach.
    let parts = filter.map(conjuncts).unwrap_or_default();
    let pinned = if verb == "PartiQLSelect" {
        key_values(&parts, &target.partition_key)
            .map(|vs| vs.into_iter().cloned().collect())
            .or_else(|| filter.and_then(|expr| pinned_values(expr, &target.partition_key)))
    } else {
        key_equality(&parts, &target.partition_key).map(|v| vec![v.clone()])
    };
    match &pinned {
        Some(values) => {
            for value in values {
                match scalar_string(value) {
                    Some(v) => keys.add_leading(v),
                    None => keys.leading = None,
                }
            }
        }
        // An UPDATE or DELETE always pins its item (the executor rejects any
        // other WHERE); a SELECT that pins nothing reads the whole table.
        None if verb != "PartiQLSelect" => keys.leading = None,
        None => {}
    }
    if verb == "PartiQLSelect" {
        // Any statement of a batch or transaction scanning the table makes the
        // request a full table scan.
        let scans = pinned.is_none();
        keys.full_table_scan = Some(keys.full_table_scan.unwrap_or(false) || scans);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn expression_attributes_are_top_level_names() {
        let n = names(&[("#s", "status"), ("#d", "detail")]);
        let mut got = expression_attributes(
            "SET #s = :v, Address.City = :c, tags[0] = :t REMOVE #d.x ADD score :one",
            &n,
        );
        got.sort();
        assert_eq!(got, ["Address", "detail", "score", "status", "tags"]);

        let mut got = expression_attributes(
            "attribute_exists(pk) AND begins_with(sk, :p) OR size(items) > :n",
            &n,
        );
        got.sort();
        assert_eq!(got, ["items", "pk", "sk"]);
    }

    #[test]
    fn partiql_set_clause_attributes_skip_literals() {
        let (_svc, state) = service_with_table();
        let got = keys_for(
            &state,
            "ExecuteStatement",
            serde_json::json!({
                "Statement": "UPDATE \"Games\" SET x = y + 1, \"note\" = 'set when? size', modified = ? WHERE UserId = 'a' AND Title = 't'",
                "Parameters": [{"S": "m"}]
            }),
        );
        assert_eq!(
            get(&got[0].1, "dynamodb:attributes"),
            Some(&["Title", "UserId", "modified", "note", "x", "y"].map(String::from)[..])
        );
    }

    #[test]
    fn query_partition_value_reads_the_key_condition() {
        let body = serde_json::json!({
            "KeyConditionExpression": "#p = :pk AND sk BETWEEN :a AND :b",
            "ExpressionAttributeNames": {"#p": "pk"},
            "ExpressionAttributeValues": {":pk": {"S": "user-1"}, ":a": {"S": "a"}, ":b": {"S": "b"}}
        });
        assert_eq!(
            query_partition_value(&body, &expression_names(&body), "pk"),
            Some("user-1".to_string())
        );
        let legacy = serde_json::json!({
            "KeyConditions": {"pk": {"ComparisonOperator": "EQ", "AttributeValueList": [{"N": "7"}]}}
        });
        assert_eq!(
            query_partition_value(&legacy, &BTreeMap::new(), "pk"),
            Some("7".to_string())
        );
    }

    fn service_with_table() -> (crate::DynamoDbService, SharedDynamoDbState) {
        let state: SharedDynamoDbState = std::sync::Arc::new(parking_lot::RwLock::new(
            fakecloud_core::multi_account::MultiAccountState::new("123456789012", "us-east-1", ""),
        ));
        let svc = crate::DynamoDbService::new(state.clone());
        let req = request(
            "CreateTable",
            serde_json::json!({
                "TableName": "Games",
                "KeySchema": [
                    {"AttributeName": "UserId", "KeyType": "HASH"},
                    {"AttributeName": "Title", "KeyType": "RANGE"}
                ],
                "AttributeDefinitions": [
                    {"AttributeName": "UserId", "AttributeType": "S"},
                    {"AttributeName": "Title", "AttributeType": "S"},
                    {"AttributeName": "Top", "AttributeType": "N"}
                ],
                "GlobalSecondaryIndexes": [{
                    "IndexName": "by-top",
                    "KeySchema": [{"AttributeName": "Title", "KeyType": "HASH"}, {"AttributeName": "Top", "KeyType": "RANGE"}],
                    "Projection": {"ProjectionType": "ALL"}
                }],
                "BillingMode": "PAY_PER_REQUEST"
            }),
        );
        svc.create_table(&req).unwrap();
        (svc, state)
    }

    fn request(action: &str, body: Value) -> AwsRequest {
        AwsRequest {
            service: "dynamodb".to_string(),
            action: action.to_string(),
            region: "us-east-1".to_string(),
            account_id: "123456789012".to_string(),
            request_id: "id".to_string(),
            headers: http::HeaderMap::new(),
            query_params: std::collections::HashMap::new(),
            body: serde_json::to_vec(&body).unwrap().into(),
            body_stream: parking_lot::Mutex::new(None),
            path_segments: vec![],
            raw_path: "/".to_string(),
            raw_query: String::new(),
            method: http::Method::POST,
            is_query_protocol: false,
            access_key_id: None,
            principal: None,
        }
    }

    /// Condition keys for every authorization a request needs, by action.
    fn keys_for(
        state: &SharedDynamoDbState,
        action: &str,
        body: Value,
    ) -> Vec<(String, BTreeMap<String, Vec<String>>)> {
        let req = request(action, body);
        super::super::iam::actions_for(state, &req)
            .into_iter()
            .map(|a| {
                let keys = condition_keys(state, &req, &a);
                (a.action.to_string(), keys)
            })
            .collect()
    }

    fn get<'a>(keys: &'a BTreeMap<String, Vec<String>>, key: &str) -> Option<&'a [String]> {
        keys.get(key).map(Vec::as_slice)
    }

    #[test]
    fn single_item_reads_and_writes() {
        let (_svc, state) = service_with_table();
        let got = keys_for(
            &state,
            "GetItem",
            serde_json::json!({
                "TableName": "Games",
                "Key": {"UserId": {"S": "alice"}, "Title": {"S": "chess"}},
                "ProjectionExpression": "#t, Stats.Wins",
                "ExpressionAttributeNames": {"#t": "Top"}
            }),
        );
        let keys = &got[0].1;
        assert_eq!(
            get(keys, "dynamodb:leadingkeys"),
            Some(&["alice".to_string()][..])
        );
        assert_eq!(
            get(keys, "dynamodb:firstpartitionkeyvalues"),
            get(keys, "dynamodb:leadingkeys")
        );
        assert_eq!(
            get(keys, "dynamodb:attributes"),
            Some(
                &[
                    "Stats".to_string(),
                    "Title".to_string(),
                    "Top".to_string(),
                    "UserId".to_string()
                ][..]
            )
        );
        assert_eq!(
            get(keys, "dynamodb:select"),
            Some(&["SPECIFIC_ATTRIBUTES".to_string()][..])
        );
        assert_eq!(
            get(keys, "dynamodb:returnconsumedcapacity"),
            Some(&["NONE".to_string()][..])
        );
        assert_eq!(get(keys, "dynamodb:returnvalues"), None);

        let got = keys_for(
            &state,
            "UpdateItem",
            serde_json::json!({
                "TableName": "Games",
                "Key": {"UserId": {"S": "bob"}, "Title": {"S": "go"}},
                "UpdateExpression": "SET Wins = Wins + :one",
                "ConditionExpression": "attribute_exists(Losses)",
                "ReturnValues": "ALL_NEW"
            }),
        );
        let keys = &got[0].1;
        assert_eq!(
            get(keys, "dynamodb:leadingkeys"),
            Some(&["bob".to_string()][..])
        );
        assert_eq!(
            get(keys, "dynamodb:attributes"),
            Some(
                &[
                    "Losses".to_string(),
                    "Title".to_string(),
                    "UserId".to_string(),
                    "Wins".to_string()
                ][..]
            )
        );
        assert_eq!(
            get(keys, "dynamodb:returnvalues"),
            Some(&["ALL_NEW".to_string()][..])
        );
        assert_eq!(get(keys, "dynamodb:select"), None);
    }

    #[test]
    fn query_uses_the_index_partition_key_and_scan_has_no_leading_keys() {
        let (_svc, state) = service_with_table();
        let got = keys_for(
            &state,
            "Query",
            serde_json::json!({
                "TableName": "Games",
                "IndexName": "by-top",
                "KeyConditionExpression": "Title = :t AND Top > :n",
                "ExpressionAttributeValues": {":t": {"S": "chess"}, ":n": {"N": "10"}}
            }),
        );
        let keys = &got[0].1;
        assert_eq!(
            get(keys, "dynamodb:leadingkeys"),
            Some(&["chess".to_string()][..])
        );
        assert_eq!(
            get(keys, "dynamodb:select"),
            Some(&["ALL_PROJECTED_ATTRIBUTES".to_string()][..])
        );

        let got = keys_for(&state, "Scan", serde_json::json!({"TableName": "Games"}));
        let keys = &got[0].1;
        assert_eq!(get(keys, "dynamodb:leadingkeys"), Some(&[][..]));
        assert_eq!(
            get(keys, "dynamodb:select"),
            Some(&["ALL_ATTRIBUTES".to_string()][..])
        );
    }

    #[test]
    fn transactions_carry_their_items_and_enclosing_operation() {
        let (_svc, state) = service_with_table();
        let got = keys_for(
            &state,
            "TransactWriteItems",
            serde_json::json!({"TransactItems": [
                {"Put": {"TableName": "Games", "Item": {"UserId": {"S": "a"}, "Title": {"S": "x"}}}},
                {"Put": {"TableName": "Games", "Item": {"UserId": {"S": "b"}, "Title": {"S": "y"}}}},
                {"Delete": {"TableName": "Games", "Key": {"UserId": {"S": "c"}, "Title": {"S": "z"}}}}
            ]}),
        );
        let put = &got.iter().find(|(a, _)| a == "PutItem").unwrap().1;
        assert_eq!(
            get(put, "dynamodb:leadingkeys"),
            Some(&["a".to_string(), "b".to_string()][..])
        );
        assert_eq!(
            get(put, "dynamodb:enclosingoperation"),
            Some(&["TransactWriteItems".to_string()][..])
        );
        let delete = &got.iter().find(|(a, _)| a == "DeleteItem").unwrap().1;
        assert_eq!(
            get(delete, "dynamodb:leadingkeys"),
            Some(&["c".to_string()][..])
        );
    }

    #[test]
    fn partiql_select_reports_full_table_scans_and_leading_keys() {
        let (_svc, state) = service_with_table();
        let got = keys_for(
            &state,
            "ExecuteStatement",
            serde_json::json!({
                "Statement": "SELECT Top FROM \"Games\" WHERE UserId = ? AND Title = 'chess'",
                "Parameters": [{"S": "alice"}]
            }),
        );
        let keys = &got[0].1;
        assert_eq!(
            get(keys, "dynamodb:leadingkeys"),
            Some(&["alice".to_string()][..])
        );
        assert_eq!(
            get(keys, "dynamodb:fulltablescan"),
            Some(&["false".to_string()][..])
        );
        assert_eq!(
            get(keys, "dynamodb:select"),
            Some(&["SPECIFIC_ATTRIBUTES".to_string()][..])
        );

        let got = keys_for(
            &state,
            "ExecuteTransaction",
            serde_json::json!({"TransactStatements": [
                {"Statement": "SELECT * FROM \"Games\" WHERE Top > 3"}
            ]}),
        );
        let keys = &got[0].1;
        assert_eq!(
            get(keys, "dynamodb:fulltablescan"),
            Some(&["true".to_string()][..])
        );
        assert_eq!(
            get(keys, "dynamodb:enclosingoperation"),
            Some(&["ExecuteTransaction".to_string()][..])
        );

        let got = keys_for(
            &state,
            "ExecuteStatement",
            serde_json::json!({"Statement": "INSERT INTO \"Games\" VALUE {'UserId': 'carol', 'Title': 't'}"}),
        );
        assert_eq!(
            get(&got[0].1, "dynamodb:leadingkeys"),
            Some(&["carol".to_string()][..])
        );
    }

    /// The resource authorized for an existing table is its own stored ARN,
    /// whatever region the request was signed for or the caller wrote into a
    /// table ARN: that table is what the handler serves.
    #[test]
    fn authorization_uses_the_stored_table_arn() {
        let (_svc, state) = service_with_table();
        let stored = "arn:aws:dynamodb:us-east-1:123456789012:table/Games";
        let mut req = request("GetItem", serde_json::json!({"TableName": "Games"}));
        req.region = "eu-west-1".to_string();
        assert_eq!(
            super::super::iam::actions_for(&state, &req)[0].resource,
            stored
        );
        let req = request(
            "GetItem",
            serde_json::json!({"TableName": "arn:aws:dynamodb:eu-west-1:123456789012:table/Games"}),
        );
        assert_eq!(
            super::super::iam::actions_for(&state, &req)[0].resource,
            stored
        );
    }

    /// Key conditions the Query handler accepts -- parenthesized, or with
    /// tabs and newlines around AND -- still yield the partition key.
    #[test]
    fn query_partition_keys_follow_the_handler_parser() {
        let (_svc, state) = service_with_table();
        for expr in [
            "(UserId = :u)",
            "UserId = :u\nAND begins_with(Title, :t)",
            "(UserId = :u)\tAND\t(Title = :t)",
        ] {
            let got = keys_for(
                &state,
                "Query",
                serde_json::json!({
                    "TableName": "Games",
                    "KeyConditionExpression": expr,
                    "ExpressionAttributeValues": {":u": {"S": "victim"}, ":t": {"S": "x"}}
                }),
            );
            assert_eq!(
                get(&got[0].1, "dynamodb:leadingkeys"),
                Some(&["victim".to_string()][..]),
                "{expr}"
            );
        }
    }

    /// Every partition a PartiQL WHERE clause can reach is reported: OR and
    /// IN widen the set, an unconstrained clause is a full table scan, and a
    /// keyword inside an identifier or a `?` inside a string literal does not
    /// throw the parse off.
    #[test]
    fn partiql_where_clauses_report_every_partition_they_reach() {
        let (_svc, state) = service_with_table();
        let leading = |statement: &str, params: Value| {
            let got = keys_for(
                &state,
                "ExecuteStatement",
                serde_json::json!({"Statement": statement, "Parameters": params}),
            );
            let keys = got[0].1.clone();
            (
                get(&keys, "dynamodb:leadingkeys").map(|v| v.to_vec()),
                get(&keys, "dynamodb:fulltablescan").map(|v| v[0].clone()),
            )
        };
        assert_eq!(
            leading(
                "SELECT * FROM \"Games\" WHERE UserId = 'mine' OR UserId = 'victim'",
                Value::Null
            ),
            (
                Some(vec!["mine".to_string(), "victim".to_string()]),
                Some("false".to_string())
            )
        );
        assert_eq!(
            leading(
                "SELECT * FROM \"Games\" WHERE UserId = 'mine' OR Top > 3",
                Value::Null
            ),
            (Some(vec![]), Some("true".to_string()))
        );
        assert_eq!(
            leading(
                "SELECT * FROM \"Games\" WHERE UserId IN ['victim']",
                Value::Null
            )
            .0,
            Some(vec!["victim".to_string()])
        );
        assert_eq!(
            leading(
                "SELECT * FROM \"Games\" WHERE (UserId = 'victim')",
                Value::Null
            )
            .0,
            Some(vec!["victim".to_string()])
        );
        assert_eq!(
            leading(
                "SELECT somewhere FROM \"Games\" WHERE UserId = 'victim'",
                Value::Null
            )
            .0,
            Some(vec!["victim".to_string()])
        );
        let got = keys_for(
            &state,
            "ExecuteStatement",
            serde_json::json!({
                "Statement": "UPDATE \"Games\" SET note = 'why?' WHERE UserId = ? AND Title = 't'",
                "Parameters": [{"S": "victim"}]
            }),
        );
        assert_eq!(
            get(&got[0].1, "dynamodb:leadingkeys"),
            Some(&["victim".to_string()][..])
        );
        assert_eq!(
            get(&got[0].1, "dynamodb:attributes"),
            Some(
                &[
                    "Title".to_string(),
                    "UserId".to_string(),
                    "note".to_string()
                ][..]
            )
        );
    }

    /// An INSERT's partition key and attributes come from the item as the
    /// executor parses it, not from the first `'UserId'` in the text.
    #[test]
    fn partiql_insert_reads_the_item() {
        let (_svc, state) = service_with_table();
        let value = "{'note': 'UserId', 'UserId': 'victim', 'Title': 't', 'secret': 'x'}";
        let got = keys_for(
            &state,
            "ExecuteStatement",
            serde_json::json!({"Statement": format!("INSERT INTO \"Games\" VALUE {value}")}),
        );
        let keys = &got[0].1;
        assert_eq!(
            get(keys, "dynamodb:leadingkeys"),
            Some(&["victim".to_string()][..])
        );
        // Exactly the attributes of the item the executor inserts.
        assert_eq!(
            get(keys, "dynamodb:attributes"),
            Some(&["Title", "UserId", "note", "secret"].map(String::from)[..])
        );
    }

    /// An INSERT whose item is a bound map parameter reports that item's
    /// partition and attributes, and a write equating the key twice reports
    /// the partition the first equality pins -- the item the executor writes.
    #[test]
    fn partiql_writes_report_what_the_executor_writes() {
        let (_svc, state) = service_with_table();
        let got = keys_for(
            &state,
            "ExecuteStatement",
            serde_json::json!({
                "Statement": "INSERT INTO \"Games\" VALUE ?",
                "Parameters": [{"M": {
                    "UserId": {"S": "victim"}, "Title": {"S": "t"}, "secret": {"S": "x"}
                }}]
            }),
        );
        assert_eq!(
            get(&got[0].1, "dynamodb:leadingkeys"),
            Some(&["victim".to_string()][..])
        );
        assert_eq!(
            get(&got[0].1, "dynamodb:attributes"),
            Some(&["Title", "UserId", "secret"].map(String::from)[..])
        );

        let got = keys_for(
            &state,
            "ExecuteStatement",
            serde_json::json!({
                "Statement": "UPDATE \"Games\" SET n = 1 WHERE UserId = 'victim' AND UserId = 'mine' AND Title = 't'"
            }),
        );
        assert_eq!(
            get(&got[0].1, "dynamodb:leadingkeys"),
            Some(&["victim".to_string()][..])
        );
    }

    /// A Scan on an index defaults to `ALL_PROJECTED_ATTRIBUTES`, like an
    /// index Query.
    #[test]
    fn index_scan_default_select_is_all_projected() {
        let (_svc, state) = service_with_table();
        let got = keys_for(
            &state,
            "Scan",
            serde_json::json!({"TableName": "Games", "IndexName": "by-top"}),
        );
        assert_eq!(
            get(&got[0].1, "dynamodb:select"),
            Some(&["ALL_PROJECTED_ATTRIBUTES".to_string()][..])
        );
    }

    /// A table ARN naming another account authorizes that account's table
    /// for an operation with cross-account support, and is authorized as
    /// written for one without it (which the handler answers not found).
    #[test]
    fn a_foreign_account_arn_authorizes_the_table_the_handler_serves() {
        let (_svc, state) = service_with_table();
        {
            let mut accounts = state.write();
            let src = accounts.get("123456789012").unwrap().tables["Games"].clone();
            let foreign = accounts.get_or_create("444455556666");
            let mut table = src;
            table.arn = "arn:aws:dynamodb:us-east-1:444455556666:table/Games".to_string();
            foreign.tables.insert("Games".to_string(), table);
        }
        let arn = "arn:aws:dynamodb:us-east-1:444455556666:table/Games";
        let req = request("GetItem", serde_json::json!({"TableName": arn}));
        assert_eq!(
            super::super::iam::actions_for(&state, &req)[0].resource,
            arn
        );
        let req = request("DescribeTimeToLive", serde_json::json!({"TableName": arn}));
        assert_eq!(
            super::super::iam::actions_for(&state, &req)[0].resource,
            arn
        );
        // A table the named account does not hold is authorized at its ARN.
        let missing = "arn:aws:dynamodb:us-east-1:444455556666:table/Nope";
        let req = request("GetItem", serde_json::json!({"TableName": missing}));
        assert_eq!(
            super::super::iam::actions_for(&state, &req)[0].resource,
            missing
        );
    }

    /// Same-named tables in two accounts named by one batch or transaction
    /// each see only the keys sent to them.
    #[test]
    fn same_named_tables_in_two_accounts_keep_their_own_keys() {
        let (_svc, state) = service_with_table();
        let arn = "arn:aws:dynamodb:us-east-1:444455556666:table/Games";
        {
            let mut accounts = state.write();
            let mut table = accounts.get("123456789012").unwrap().tables["Games"].clone();
            table.arn = arn.to_string();
            accounts
                .get_or_create("444455556666")
                .tables
                .insert("Games".to_string(), table);
        }
        let own = "arn:aws:dynamodb:us-east-1:123456789012:table/Games";
        for (action, body) in [
            (
                "TransactWriteItems",
                serde_json::json!({"TransactItems": [
                    {"Put": {"TableName": "Games", "Item": {"UserId": {"S": "mine"}, "Title": {"S": "a"}}}},
                    {"Put": {"TableName": arn, "Item": {"UserId": {"S": "theirs"}, "Title": {"S": "a"}}}}
                ]}),
            ),
            (
                "BatchGetItem",
                serde_json::json!({"RequestItems": {
                    "Games": {"Keys": [{"UserId": {"S": "mine"}, "Title": {"S": "a"}}]},
                    arn: {"Keys": [{"UserId": {"S": "theirs"}, "Title": {"S": "a"}}]}
                }}),
            ),
        ] {
            let req = request(action, body);
            let actions = super::super::iam::actions_for(&state, &req);
            let leading = |resource: &str| {
                let a = actions.iter().find(|a| a.resource == resource)?;
                condition_keys(&state, &req, a)
                    .get("dynamodb:leadingkeys")
                    .cloned()
            };
            assert_eq!(leading(own), Some(vec!["mine".to_string()]), "{action}");
            assert_eq!(leading(arn), Some(vec!["theirs".to_string()]), "{action}");
        }
    }

    /// A key condition parenthesized as a whole still yields its partition
    /// key, a number key is reported canonically, and a condition the
    /// extraction cannot read leaves the key unknown (omitted) rather than an
    /// empty set a `ForAllValues` would accept.
    #[test]
    fn leading_keys_are_known_values_known_empty_or_omitted() {
        let (_svc, state) = service_with_table();
        let got = keys_for(
            &state,
            "Query",
            serde_json::json!({
                "TableName": "Games",
                "KeyConditionExpression": "(UserId = :u AND Title = :t)",
                "ExpressionAttributeValues": {":u": {"S": "victim"}, ":t": {"S": "x"}}
            }),
        );
        assert_eq!(
            get(&got[0].1, "dynamodb:leadingkeys"),
            Some(&["victim".to_string()][..])
        );

        let got = keys_for(
            &state,
            "Query",
            serde_json::json!({
                "TableName": "Games",
                "KeyConditionExpression": "UserId = :u",
                "ExpressionAttributeValues": {}
            }),
        );
        assert_eq!(
            get(&got[0].1, "dynamodb:leadingkeys"),
            None,
            "unknown is omitted"
        );

        let got = keys_for(&state, "Scan", serde_json::json!({"TableName": "Games"}));
        assert_eq!(
            get(&got[0].1, "dynamodb:leadingkeys"),
            Some(&[][..]),
            "a scan pins none"
        );
        assert_eq!(get(&got[0].1, "dynamodb:attributes"), Some(&[][..]));

        let got = keys_for(
            &state,
            "ExecuteStatement",
            serde_json::json!({"Statement": "SELECT * FROM \"Games\" WHERE UserId = 1.0"}),
        );
        assert_eq!(
            get(&got[0].1, "dynamodb:leadingkeys"),
            Some(&["1".to_string()][..])
        );
    }

    /// One scanning statement makes a batch or transaction a full table scan,
    /// whatever order the statements come in; dotted attribute names are
    /// reported whole, as the executor reads them.
    #[test]
    fn full_table_scan_and_dotted_names_across_statements() {
        let (_svc, state) = service_with_table();
        let got = keys_for(
            &state,
            "ExecuteTransaction",
            serde_json::json!({"TransactStatements": [
                {"Statement": "SELECT * FROM \"Games\" WHERE Top > 3"},
                {"Statement": "SELECT * FROM \"Games\" WHERE UserId = 'mine' AND \"a.b\" = 1"}
            ]}),
        );
        let keys = &got[0].1;
        assert_eq!(
            get(keys, "dynamodb:fulltablescan"),
            Some(&["true".to_string()][..])
        );
        let attrs = get(keys, "dynamodb:attributes").unwrap();
        assert!(attrs.contains(&"a.b".to_string()), "{attrs:?}");
    }

    /// SELECT and SET columns are read with the executor's parsers, so names
    /// that look like keywords, start with a digit or carry `#`/`:` are all
    /// reported; and `Select` is the most permissive across statements.
    #[test]
    fn partiql_columns_and_select_follow_the_executor() {
        let (_svc, state) = service_with_table();
        let got = keys_for(
            &state,
            "ExecuteStatement",
            serde_json::json!({
                "Statement": "SELECT size, value, 2fa_secret, #secret, \"a:b\" FROM \"Games\" WHERE UserId = 'mine'"
            }),
        );
        let attrs = get(&got[0].1, "dynamodb:attributes").unwrap().to_vec();
        for name in ["size", "value", "2fa_secret", "#secret", "a:b", "UserId"] {
            assert!(
                attrs.contains(&name.to_string()),
                "{name} missing from {attrs:?}"
            );
        }

        let got = keys_for(
            &state,
            "ExecuteStatement",
            serde_json::json!({
                "Statement": "UPDATE \"Games\" SET modified = ?, \"new\" = 1 WHERE UserId = 'mine' AND Title = 't'",
                "Parameters": [{"S": "x"}]
            }),
        );
        let attrs = get(&got[0].1, "dynamodb:attributes").unwrap().to_vec();
        assert!(attrs.contains(&"modified".to_string()), "{attrs:?}");
        assert!(attrs.contains(&"new".to_string()), "{attrs:?}");

        let got = keys_for(
            &state,
            "BatchExecuteStatement",
            serde_json::json!({"Statements": [
                {"Statement": "SELECT * FROM \"Games\" WHERE UserId = 'mine'"},
                {"Statement": "SELECT UserId FROM \"Games\" WHERE UserId = 'mine'"}
            ]}),
        );
        assert_eq!(
            get(&got[0].1, "dynamodb:select"),
            Some(&["ALL_ATTRIBUTES".to_string()][..])
        );

        let got = keys_for(
            &state,
            "TransactGetItems",
            serde_json::json!({"TransactItems": [
                {"Get": {"TableName": "Games", "Key": {"UserId": {"S": "a"}, "Title": {"S": "x"}}}},
                {"Get": {"TableName": "Games", "Key": {"UserId": {"S": "a"}, "Title": {"S": "y"}}, "ProjectionExpression": "UserId"}}
            ]}),
        );
        assert_eq!(
            get(&got[0].1, "dynamodb:select"),
            Some(&["ALL_ATTRIBUTES".to_string()][..])
        );
    }

    /// Every way the executor can read an update target is reported: the
    /// first segment and the whole path, names resolved.
    #[test]
    fn update_targets_report_every_reading_of_a_path() {
        let names: BTreeMap<String, String> = [("#m".to_string(), "meta".to_string())]
            .into_iter()
            .collect();
        let mut got = update_targets(
            "SET \"a.b\" = :v, x.y[0] = :w ADD #m.c :n REMOVE z[1][2]",
            &names,
        );
        got.sort();
        got.dedup();
        for want in ["a", "a.b", "x", "x.y[0]", "meta", "meta.c", "z", "z[1][2]"] {
            assert!(
                got.contains(&want.to_string()),
                "{want} missing from {got:?}"
            );
        }
    }

    /// A plain placeholder target reports only the attribute it names.
    #[test]
    fn update_targets_resolve_placeholders_without_extra_names() {
        let names: BTreeMap<String, String> = [("#t".to_string(), "Top".to_string())]
            .into_iter()
            .collect();
        let mut got = update_targets("SET #t = :v, Wins = Wins + :one", &names);
        got.sort();
        got.dedup();
        assert_eq!(got, ["Top", "Wins"]);
    }
}
