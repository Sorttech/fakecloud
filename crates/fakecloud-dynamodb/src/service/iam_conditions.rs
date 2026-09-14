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

use super::helpers::partiql::find_outside_quotes;

/// Words in DynamoDB and PartiQL expressions that are never attribute names.
const EXPRESSION_WORDS: &[&str] = &[
    "and",
    "or",
    "not",
    "between",
    "in",
    "set",
    "remove",
    "add",
    "delete",
    "select",
    "from",
    "where",
    "value",
    "values",
    "into",
    "update",
    "insert",
    "returning",
    "all",
    "old",
    "new",
    "modified",
    "true",
    "false",
    "null",
    "missing",
    "is",
    "exists",
    "size",
];

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
    table_ref_name: String,
    partition_key: String,
    index: Option<String>,
}

fn target(
    accounts: &fakecloud_core::multi_account::MultiAccountState<crate::state::DynamoDbState>,
    resource: &str,
) -> Option<Target> {
    let rest = resource.strip_prefix("arn:aws:dynamodb:")?;
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
        table_ref_name: name.to_string(),
        partition_key,
        index,
    })
}

/// Whether a request's `TableName` value names `target`'s table.
fn names_table(target: &Target, table_name: Option<&str>) -> bool {
    let Some(name) = table_name else {
        return false;
    };
    let resolved = super::resolve_table_name(name);
    resolved == target.table_ref_name
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
                keys.select = Some(implicit_select(&body, false));
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
                keys.select = Some(implicit_select(&body, t.index.is_some()));
            }
            add_request_attributes(&mut keys, &body, &names);
            keys.return_consumed_capacity = rcc();
        }
        "Scan" => {
            add_request_attributes(&mut keys, &body, &names);
            keys.select = Some(implicit_select(&body, body["IndexName"].is_string()));
            keys.return_consumed_capacity = rcc();
        }
        "BatchGetItem" | "BatchWriteItem" => {
            if let (Some(t), Some(items)) = (&target, body["RequestItems"].as_object()) {
                for (table_name, entry) in items {
                    if !names_table(t, Some(table_name)) {
                        continue;
                    }
                    if request.action == "BatchGetItem" {
                        let entry_names = expression_names(entry);
                        for key in entry["Keys"].as_array().into_iter().flatten() {
                            add_leading(&mut keys, key, &t.partition_key);
                            add_item_attributes(&mut keys, key);
                        }
                        add_request_attributes(&mut keys, entry, &entry_names);
                        keys.select = Some(implicit_select(entry, false));
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
                    if !names_table(t, op["TableName"].as_str()) {
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
                        keys.select = Some(implicit_select(op, false));
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
    for expr in [
        "ProjectionExpression",
        "UpdateExpression",
        "ConditionExpression",
        "FilterExpression",
        "KeyConditionExpression",
    ] {
        if let Some(text) = body[expr].as_str() {
            keys.attributes
                .extend(expression_attributes(text, names, false));
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
/// not a keyword, a function call, a nested path segment or a list index. In
/// a PartiQL statement (`partiql`) the table -- and a `"table"."index"` index
/// -- after `FROM` / `INTO` / `UPDATE` is not an attribute either.
fn expression_attributes(
    text: &str,
    names: &BTreeMap<String, String>,
    partiql: bool,
) -> Vec<String> {
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
        if partiql && !quoted && matches!(lower.as_str(), "from" | "into" | "update") {
            // The keyword and the table name, then an optional `.index`.
            i += 2;
            if matches!(tokens.get(i), Some(Token::Symbol('.'))) {
                i += 2;
            }
            continue;
        }
        let keyword = !quoted && EXPRESSION_WORDS.contains(&lower.as_str());
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

/// The PartiQL condition keys for one statement, parsed exactly the way the
/// executor parses it -- the same clause splitting, the same `?` parameter
/// binding, the same WHERE parser and item parser -- so a statement the
/// executor accepts cannot be read here as touching different partitions or
/// attributes than it does.
fn add_partiql_keys(
    keys: &mut Keys,
    target: &Target,
    statement: &str,
    parameters: &[Value],
    verb: &str,
) {
    use super::helpers::count_params_in_str;
    use super::helpers::partiql::{
        parse_partiql_table_name, parse_partiql_value_object, partiql_expr_attributes,
        partiql_pinned_values, partiql_where_conditions, split_partiql_returning_clause,
    };

    let trimmed = statement.trim();
    let upper = trimmed.to_ascii_uppercase();
    let mut conditions = None;
    match verb {
        "PartiQLInsert" => {
            let Some(into) = find_outside_quotes(&upper, "INTO") else {
                return;
            };
            let (_, rest) = parse_partiql_table_name(trimmed[into + 4..].trim());
            let rest_upper = rest.trim().to_ascii_uppercase();
            let Some(value_pos) = find_outside_quotes(&rest_upper, "VALUE") else {
                return;
            };
            let value_str = rest.trim()[value_pos + 5..].trim();
            match parse_partiql_value_object(value_str, parameters) {
                Ok(item) => {
                    match item.get(&target.partition_key).and_then(scalar_string) {
                        Some(v) => keys.add_leading(v),
                        None => keys.leading = None,
                    }
                    keys.attributes.extend(item.keys().cloned());
                }
                Err(_) => keys.leading = None,
            }
            return;
        }
        "PartiQLSelect" => {
            let Some(from) = find_outside_quotes(&upper, "FROM") else {
                return;
            };
            let projection = trimmed["SELECT".len()..from].trim();
            keys.attributes
                .extend(expression_attributes(projection, &BTreeMap::new(), true));
            keys.select = Some(if projection == "*" {
                if target.index.is_some() {
                    "ALL_PROJECTED_ATTRIBUTES".to_string()
                } else {
                    "ALL_ATTRIBUTES".to_string()
                }
            } else {
                "SPECIFIC_ATTRIBUTES".to_string()
            });
            let (_, mut rest) = parse_partiql_table_name(trimmed[from + 4..].trim());
            if let Some(index_part) = rest.strip_prefix('.') {
                rest = parse_partiql_table_name(index_part).1;
            }
            if rest.trim().to_ascii_uppercase().starts_with("WHERE") {
                conditions = partiql_where_conditions(rest.trim()[5..].trim(), parameters);
            }
        }
        "PartiQLUpdate" => {
            let (_, rest) = parse_partiql_table_name(trimmed[6..].trim());
            let rest_upper = rest.trim().to_ascii_uppercase();
            let Some(set_pos) = find_outside_quotes(&rest_upper, "SET") else {
                return;
            };
            let (after_set, _) = split_partiql_returning_clause(rest.trim()[set_pos + 3..].trim());
            let (set_clause, where_clause) =
                match find_outside_quotes(&after_set.to_ascii_uppercase(), "WHERE") {
                    Some(wp) => (&after_set[..wp], after_set[wp + 5..].trim()),
                    None => (after_set, ""),
                };
            keys.attributes
                .extend(expression_attributes(set_clause, &BTreeMap::new(), true));
            let set_params = count_params_in_str(set_clause);
            let where_params = parameters.get(set_params..).unwrap_or(&[]);
            conditions = partiql_where_conditions(where_clause, where_params);
        }
        "PartiQLDelete" => {
            let Some(from) = find_outside_quotes(&upper, "FROM") else {
                return;
            };
            let (_, rest) = parse_partiql_table_name(trimmed[from + 4..].trim());
            if rest.trim().to_ascii_uppercase().starts_with("WHERE") {
                conditions = partiql_where_conditions(rest.trim()[5..].trim(), parameters);
            }
        }
        _ => return,
    }
    if let Some(expr) = &conditions {
        let mut attrs = Vec::new();
        partiql_expr_attributes(expr, &mut attrs);
        keys.attributes.extend(attrs);
    }
    let pinned = conditions
        .as_ref()
        .and_then(|expr| partiql_pinned_values(expr, &target.partition_key));
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
            false,
        );
        got.sort();
        assert_eq!(got, ["Address", "detail", "score", "status", "tags"]);

        let mut got = expression_attributes(
            "attribute_exists(pk) AND begins_with(sk, :p) OR size(items) > :n",
            &n,
            false,
        );
        got.sort();
        assert_eq!(got, ["items", "pk", "sk"]);
    }

    #[test]
    fn partiql_attributes_skip_table_and_literals() {
        let mut got = expression_attributes(
            "SELECT name, \"Address\".city FROM \"Orders\".\"by-customer\" WHERE pk = 'a' AND qty > 3",
            &BTreeMap::new(),
            true,
        );
        got.sort();
        assert_eq!(got, ["Address", "name", "pk", "qty"]);
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
        // Exactly the attributes the executor's item parser finds.
        let item = super::super::helpers::partiql::parse_partiql_value_object(value, &[]).unwrap();
        let mut expected: Vec<String> = item.keys().cloned().collect();
        expected.sort();
        assert_eq!(get(keys, "dynamodb:attributes"), Some(&expected[..]));
        assert!(expected.contains(&"secret".to_string()));
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

    /// A table ARN naming another account still authorizes the caller's own
    /// table: that is the table the handler serves.
    #[test]
    fn a_foreign_account_arn_authorizes_the_callers_table() {
        let (_svc, state) = service_with_table();
        let req = request(
            "GetItem",
            serde_json::json!({"TableName": "arn:aws:dynamodb:us-east-1:444455556666:table/Games"}),
        );
        assert_eq!(
            super::super::iam::actions_for(&state, &req)[0].resource,
            "arn:aws:dynamodb:us-east-1:123456789012:table/Games"
        );
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
}
