//! Vector indexes and `SearchVectors`.
//!
//! A vector index names one list-valued attribute on the table and keeps its
//! own copy of every item that carries a well-formed vector there: the table
//! and search-schema keys, the attributes it projects, and the vector itself
//! narrowed to 32-bit floats (the index's storage precision). That copy -- the
//! index *entry* -- is what `SearchVectors` scores and returns, what a write is
//! charged for, and what `ItemCount` / `IndexSizeBytes` describe.
//!
//! Scoring is real: cosine distance, dot product or Euclidean distance over the
//! f32 values, computed exactly, so the ranking and the scores are the ones the
//! stored vectors imply.

use std::collections::BTreeMap;

use base64::Engine as _;
use chrono::Utc;

use crate::state::{DynamoTable, VectorIndex, VectorIndexPhase};

use super::*;

/// Largest `TopK` a search accepts.
const MAX_TOP_K: i64 = 100;
/// Floor applied to each index's `VectorWriteRequestBytes`.
const VECTOR_WRITE_FLOOR_BYTES: f64 = 1024.0;
/// Floor applied to a search's `VectorSearchRequestBytes`.
const VECTOR_SEARCH_FLOOR_BYTES: f64 = 1024.0;

type Item = HashMap<String, AttributeValue>;

fn validation(message: impl Into<String>) -> AwsServiceError {
    AwsServiceError::aws_error(StatusCode::BAD_REQUEST, "ValidationException", message)
}

/// Parse one `N` value into the f32 the index stores, or `None` when the value
/// is not a number a 32-bit float can hold.
fn as_f32(value: &AttributeValue) -> Option<f32> {
    let n: f64 = value.get("N")?.as_str()?.parse().ok()?;
    let f = n as f32;
    f.is_finite().then_some(f)
}

/// An item's vector as the index stores it, when it is well formed for `index`.
fn indexed_vector(index: &VectorIndex, item: &Item) -> Option<Vec<f32>> {
    let list = item.get(&index.vector_attribute)?.get("L")?.as_array()?;
    if list.len() as i64 != index.dimensions {
        return None;
    }
    list.iter().map(as_f32).collect()
}

/// Whether `value` is an empty string or empty binary, which no index key may hold.
fn is_empty_key_value(value: &AttributeValue) -> bool {
    value.get("S").and_then(Value::as_str) == Some("")
        || value.get("B").and_then(Value::as_str) == Some("")
}

/// The index's entry for `item`, or `None` when the item is not in the index:
/// its vector is absent or malformed, or it lacks the search schema's HASH
/// attribute (a documented silent de-index -- the write succeeds and the item
/// is simply unreachable through this index).
pub(crate) fn vector_entry(table: &DynamoTable, index: &VectorIndex, item: &Item) -> Option<Item> {
    let vector = indexed_vector(index, item)?;
    for (attr, kind) in &index.search_schema {
        if kind == "HASH" && item.get(attr).is_none_or(is_empty_key_value) {
            return None;
        }
    }
    let mut entry = Item::new();
    let keys = table
        .key_schema
        .iter()
        .map(|k| k.attribute_name.as_str())
        .chain(index.search_schema.iter().map(|(a, _)| a.as_str()));
    for attr in keys {
        if let Some(v) = item.get(attr) {
            entry.insert(attr.to_string(), v.clone());
        }
    }
    match index.projection.projection_type.as_str() {
        "ALL" => {
            for (k, v) in item {
                entry.entry(k.clone()).or_insert_with(|| v.clone());
            }
        }
        "INCLUDE" => {
            for attr in &index.projection.non_key_attributes {
                if let Some(v) = item.get(attr) {
                    entry.insert(attr.clone(), v.clone());
                }
            }
        }
        _ => {}
    }
    // The index keeps its own f32 copy, serialised as the shortest decimal
    // that names each float.
    let narrowed: Vec<Value> = vector
        .iter()
        .map(|f| json!({ "N": format!("{f}") }))
        .collect();
    entry.insert(index.vector_attribute.clone(), json!({ "L": narrowed }));
    Some(entry)
}

/// Bytes a DynamoDB number occupies: one byte, plus one per two significant
/// digits on each side of the decimal point, plus one for a negative sign.
fn number_bytes(literal: &str) -> usize {
    let literal = literal.trim();
    let (negative, unsigned) = match literal.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, literal.strip_prefix('+').unwrap_or(literal)),
    };
    let (mantissa, exponent) = match unsigned.find(['e', 'E']) {
        Some(i) => (
            &unsigned[..i],
            unsigned[i + 1..].parse::<i64>().unwrap_or(0),
        ),
        None => (unsigned, 0),
    };
    let (int_text, frac_text) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let digits = format!("{int_text}{frac_text}");
    if !digits.bytes().any(|b| (b'1'..=b'9').contains(&b)) {
        return 1;
    }
    let point = int_text.len() as i64 + exponent;
    let (int_part, frac_part) = if point <= 0 {
        (
            String::new(),
            format!("{}{digits}", "0".repeat((-point) as usize)),
        )
    } else if point as usize >= digits.len() {
        (
            format!("{digits}{}", "0".repeat(point as usize - digits.len())),
            String::new(),
        )
    } else {
        (
            digits[..point as usize].to_string(),
            digits[point as usize..].to_string(),
        )
    };
    let int_part = int_part.trim_start_matches('0');
    let frac_part = frac_part.trim_end_matches('0');
    let int_significant = if frac_part.is_empty() {
        int_part.trim_end_matches('0').len()
    } else {
        int_part.len()
    };
    let frac_significant = if int_part.is_empty() {
        frac_part.trim_start_matches('0').len()
    } else {
        frac_part.len()
    };
    1 + int_significant.div_ceil(2) + frac_significant.div_ceil(2) + usize::from(negative)
}

fn base64_len(b64: &str) -> usize {
    base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map(|b| b.len())
        .unwrap_or(b64.len())
}

/// Bytes one attribute value contributes to an item's size.
fn value_bytes(value: &AttributeValue) -> usize {
    let Some(obj) = value.as_object() else {
        return 0;
    };
    if let Some(s) = obj.get("S").and_then(Value::as_str) {
        s.len()
    } else if let Some(n) = obj.get("N").and_then(Value::as_str) {
        number_bytes(n)
    } else if let Some(b) = obj.get("B").and_then(Value::as_str) {
        base64_len(b)
    } else if obj.contains_key("BOOL") || obj.contains_key("NULL") {
        1
    } else if let Some(ss) = obj.get("SS").and_then(Value::as_array) {
        ss.iter().filter_map(Value::as_str).map(str::len).sum()
    } else if let Some(ns) = obj.get("NS").and_then(Value::as_array) {
        ns.iter().filter_map(Value::as_str).map(number_bytes).sum()
    } else if let Some(bs) = obj.get("BS").and_then(Value::as_array) {
        bs.iter().filter_map(Value::as_str).map(base64_len).sum()
    } else if let Some(l) = obj.get("L").and_then(Value::as_array) {
        3 + l.iter().map(|v| 1 + value_bytes(v)).sum::<usize>()
    } else if let Some(m) = obj.get("M").and_then(Value::as_object) {
        3 + m
            .iter()
            .map(|(k, v)| 1 + k.len() + value_bytes(v))
            .sum::<usize>()
    } else {
        0
    }
}

/// The size of an index entry. The vector attribute is metered as its name plus
/// a flat four bytes per dimension; every other attribute at its item size.
fn entry_bytes(index: &VectorIndex, entry: &Item) -> usize {
    entry
        .iter()
        .map(|(name, value)| {
            if *name == index.vector_attribute {
                name.len() + 4 * index.dimensions as usize
            } else {
                name.len() + value_bytes(value)
            }
        })
        .sum()
}

/// `(ItemCount, IndexSizeBytes)` for a vector index.
pub(crate) fn vector_index_stats(table: &DynamoTable, index: &VectorIndex) -> (i64, i64) {
    table
        .items
        .iter()
        .filter_map(|item| vector_entry(table, index, item))
        .fold((0, 0), |(count, bytes), entry| {
            (count + 1, bytes + entry_bytes(index, &entry) as i64)
        })
}

/// Per-index `VectorWriteRequestBytes` for a write that turns `old` into `new`
/// (either side `None` for an insert or a delete).
///
/// Replication is delta-based: an index is charged only when its entry for the
/// item changes, and then the size of the entry it now holds (or held, for a
/// removal), held to a 1024-byte floor per index.
pub(crate) fn vector_write_charges(
    table: &DynamoTable,
    old: Option<&Item>,
    new: Option<&Item>,
) -> BTreeMap<String, f64> {
    let mut charges = BTreeMap::new();
    for index in &table.vector_indexes {
        let before = old.and_then(|i| vector_entry(table, index, i));
        let after = new.and_then(|i| vector_entry(table, index, i));
        if before == after {
            continue;
        }
        let Some(charged) = after.as_ref().or(before.as_ref()) else {
            continue;
        };
        let bytes = (entry_bytes(index, charged) as f64).max(VECTOR_WRITE_FLOOR_BYTES);
        charges.insert(index.index_name.clone(), bytes);
    }
    charges
}

/// The type tag of an attribute value (`S`, `N`, `L`, ...).
fn type_tag(value: &AttributeValue) -> &str {
    value
        .as_object()
        .and_then(|o| o.keys().next())
        .map(String::as_str)
        .unwrap_or("")
}

/// Reject a write whose item carries a malformed vector, or an unusable
/// search-schema key, for one of the table's vector indexes.
pub(crate) fn validate_vector_item(
    indexes: &[VectorIndex],
    attribute_definitions: &[crate::state::AttributeDefinition],
    item: &Item,
) -> Result<(), AwsServiceError> {
    for index in indexes {
        let name = &index.index_name;
        if let Some(value) = item.get(&index.vector_attribute) {
            let attr = &index.vector_attribute;
            let Some(list) = value.get("L").and_then(Value::as_array) else {
                return Err(validation(format!(
                    "One or more parameter values were invalid. Invalid type for parameter \
                     {attr}, Expected: 32-bit floating point number list IndexName: {name}"
                )));
            };
            for (i, element) in list.iter().enumerate() {
                if as_f32(element).is_none() {
                    return Err(validation(format!(
                        "One or more parameter values were invalid. Invalid type for parameter \
                         {attr}[{i}], Expected: 32-bit floating point number, Actual: {}. \
                         IndexName: {name}",
                        type_tag(element)
                    )));
                }
            }
            if list.len() as i64 != index.dimensions {
                return Err(validation(format!(
                    "One or more parameter values were invalid. Invalid size for parameter \
                     {attr}, Expected: {}, Actual: {} IndexName: {name}",
                    index.dimensions,
                    list.len()
                )));
            }
        }
        for (attr, _) in &index.search_schema {
            let Some(value) = item.get(attr) else {
                continue;
            };
            if is_empty_key_value(value) {
                return Err(validation(format!(
                    "One or more parameter values are not valid. A value specified for a \
                     secondary index key is not supported. The AttributeValue for a key \
                     attribute cannot contain an empty string value. IndexName: {name}, \
                     IndexKey: {attr}"
                )));
            }
            if let Some(def) = attribute_definitions
                .iter()
                .find(|d| d.attribute_name == *attr)
            {
                let actual = type_tag(value);
                if actual != def.attribute_type {
                    return Err(validation(format!(
                        "One or more parameter values were invalid: Type mismatch for Index Key \
                         {attr} Expected: {} Actual: {actual} IndexName: {name}",
                        def.attribute_type
                    )));
                }
            }
        }
    }
    Ok(())
}

/// The rejection for a Scan or Query naming a vector index, which neither can read.
pub(crate) fn reject_vector_index_read(
    table: &DynamoTable,
    index_name: &str,
    operation: &str,
) -> Result<(), AwsServiceError> {
    if table
        .vector_indexes
        .iter()
        .any(|v| v.index_name == index_name)
    {
        return Err(validation(format!(
            "{operation} operation not supported on this index type"
        )));
    }
    Ok(())
}

/// Score `candidate` against `query` under `function`. COSINE and EUCLIDEAN are
/// distances (lower is closer); DOT_PRODUCT is a similarity (higher is closer).
/// The result is narrowed to f32, the precision the index computes in.
fn score(function: &str, query: &[f32], candidate: &[f32]) -> f32 {
    let dot = |a: &[f32], b: &[f32]| -> f64 {
        a.iter()
            .zip(b)
            .map(|(x, y)| f64::from(*x) * f64::from(*y))
            .sum()
    };
    let s = match function {
        "DOT_PRODUCT" => dot(query, candidate),
        "EUCLIDEAN" => query
            .iter()
            .zip(candidate)
            .map(|(x, y)| (f64::from(*x) - f64::from(*y)).powi(2))
            .sum::<f64>()
            .sqrt(),
        _ => {
            let denom = dot(query, query).sqrt() * dot(candidate, candidate).sqrt();
            if denom == 0.0 {
                1.0
            } else {
                1.0 - dot(query, candidate) / denom
            }
        }
    };
    s as f32
}

/// One `attr = :value` term of a `SearchConditionExpression`.
struct SearchTerm {
    attribute: String,
    value: AttributeValue,
}

const INVALID_COMPARATOR: &str =
    "Invalid SearchConditionExpression: Invalid comparator used in SearchConditionExpression";

/// Split `expr` on top-level, case-insensitive `AND`.
fn split_and(expr: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let bytes = expr.as_bytes();
    let mut depth = 0i32;
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'(' => depth += 1,
            b')' => depth -= 1,
            _ => {}
        }
        let boundary_before = i == 0 || bytes[i - 1].is_ascii_whitespace() || bytes[i - 1] == b')';
        if depth == 0
            && boundary_before
            && i + 3 <= bytes.len()
            && expr[i..i + 3].eq_ignore_ascii_case("AND")
            && (i + 3 == bytes.len() || bytes[i + 3].is_ascii_whitespace() || bytes[i + 3] == b'(')
        {
            parts.push(&expr[start..i]);
            start = i + 3;
            i += 3;
            continue;
        }
        i += 1;
    }
    parts.push(&expr[start..]);
    parts
}

/// Parse a `SearchConditionExpression`: a conjunction of equality conditions on
/// search-schema attributes. Every other comparator is refused, on HASH and
/// INLINE_FILTER elements alike.
fn parse_search_condition(
    expr: &str,
    index: &VectorIndex,
    names: &HashMap<String, String>,
    values: &HashMap<String, Value>,
) -> Result<Vec<SearchTerm>, AwsServiceError> {
    let mut terms = Vec::new();
    for raw in split_and(expr) {
        let term = strip_outer_parens(raw.trim()).trim();
        if term.is_empty() {
            return Err(validation(
                "Invalid SearchConditionExpression: Syntax error; empty condition",
            ));
        }
        let Some(eq) = term.find('=') else {
            return Err(validation(INVALID_COMPARATOR));
        };
        let (lhs, rhs) = (term[..eq].trim(), term[eq + 1..].trim());
        // `<=`, `>=` and `<>` all contain or border an `=`.
        if lhs.ends_with(['<', '>', '!']) || rhs.starts_with(['<', '>', '=']) {
            return Err(validation(INVALID_COMPARATOR));
        }
        let is_operand = |s: &str| {
            !s.is_empty()
                && s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '#' | ':' | '.' | '-'))
        };
        if !is_operand(lhs) || !is_operand(rhs) {
            return Err(validation(INVALID_COMPARATOR));
        }
        let (path, placeholder) = if rhs.starts_with(':') {
            (lhs, rhs)
        } else if lhs.starts_with(':') {
            (rhs, lhs)
        } else {
            return Err(validation(INVALID_COMPARATOR));
        };
        let attribute = if path.starts_with('#') {
            names.get(path).cloned().ok_or_else(|| {
                validation(format!(
                    "Invalid SearchConditionExpression: An expression attribute name used in \
                     the document path is not defined; attribute name: {path}"
                ))
            })?
        } else {
            path.to_string()
        };
        if !index.search_schema.iter().any(|(a, _)| *a == attribute) {
            return Err(validation(format!(
                "SearchConditionExpression must not contain any attributes that is not in \
                 SearchSchema. Invalid attribute: {attribute}"
            )));
        }
        let value = values.get(placeholder).cloned().ok_or_else(|| {
            validation(format!(
                "Invalid SearchConditionExpression: An expression attribute value used in \
                 expression is not defined; attribute value: {placeholder}"
            ))
        })?;
        terms.push(SearchTerm { attribute, value });
    }
    Ok(terms)
}

/// A `SearchVector` element as a query value, when it is a plain `N`.
fn search_vector(list: &[Value]) -> Option<Vec<f32>> {
    list.iter().map(as_f32).collect()
}

impl DynamoDbService {
    pub(super) fn search_vectors(&self, req: &AwsRequest) -> Result<AwsResponse, AwsServiceError> {
        let body = Self::parse_body(req)?;

        // Request-model layer.
        let mut model = ModelErrors::default();
        match body["TableName"].as_str() {
            Some(name) if name.is_empty() || name.len() > 1024 => model.push(format!(
                "Value '{name}' at 'tableName' failed to satisfy constraint: Member must have \
                 length {}",
                if name.is_empty() {
                    "greater than or equal to 1"
                } else {
                    "less than or equal to 1024"
                }
            )),
            Some(_) => {}
            None => model.push(
                "Value null at 'tableName' failed to satisfy constraint: Member must not be null"
                    .to_string(),
            ),
        }
        match body["IndexName"].as_str() {
            Some(name) => {
                let len = name.chars().count();
                if len < 3 {
                    model.push(format!(
                        "Value '{name}' at 'indexName' failed to satisfy constraint: Member must \
                         have length greater than or equal to 3"
                    ));
                }
                if len > 255 {
                    model.push(format!(
                        "Value '{name}' at 'indexName' failed to satisfy constraint: Member must \
                         have length less than or equal to 255"
                    ));
                }
                if !name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
                {
                    model.push(format!(
                        "Value '{name}' at 'indexName' failed to satisfy constraint: Member must \
                         satisfy regular expression pattern: [a-zA-Z0-9_.-]+"
                    ));
                }
            }
            None => model.push(
                "Value null at 'indexName' failed to satisfy constraint: Member must not be null"
                    .to_string(),
            ),
        }
        match body["SearchVector"].as_array() {
            Some(v) if v.is_empty() => model.push(
                "Value '[]' at 'searchVector' failed to satisfy constraint: Member must have \
                 length greater than or equal to 1"
                    .to_string(),
            ),
            Some(v) if v.len() > 4096 => model.push(
                "Value at 'searchVector' failed to satisfy constraint: Member must have length \
                 less than or equal to 4096"
                    .to_string(),
            ),
            Some(_) => {}
            None => model.push(
                "Value null at 'searchVector' failed to satisfy constraint: Member must not be \
                 null"
                    .to_string(),
            ),
        }
        match body["TopK"].as_i64() {
            Some(k) if k < 1 => model.push(format!(
                "Value '{k}' at 'topK' failed to satisfy constraint: Member must have value \
                 greater than or equal to 1"
            )),
            Some(_) => {}
            None => model.push(
                "Value null at 'topK' failed to satisfy constraint: Member must not be null"
                    .to_string(),
            ),
        }
        if let Some(mode) = body["ReturnConsumedCapacity"].as_str() {
            if !["INDEXES", "TOTAL", "NONE"].contains(&mode) {
                model.push(format!(
                    "Value '{mode}' at 'returnConsumedCapacity' failed to satisfy constraint: \
                     Member must satisfy enum value set: [INDEXES, TOTAL, NONE]"
                ));
            }
        }
        model.into_result()?;
        // A malformed or overlapping projection is rejected up front, as on
        // Query and Scan, rather than projecting nothing.
        validate_read_projection(&body)?;

        let table_name = body["TableName"].as_str().unwrap_or_default();
        let index_name = body["IndexName"].as_str().unwrap_or_default();
        let top_k = body["TopK"].as_i64().unwrap_or_default();
        if top_k > MAX_TOP_K {
            return Err(validation(format!(
                "Provided TopK value '{top_k}' is out of valid range. The value must be between \
                 1 and {MAX_TOP_K} inclusive"
            )));
        }
        let query_vector = search_vector(body["SearchVector"].as_array().map_or(&[], |v| v))
            .ok_or_else(|| {
                validation(
                    "Search vector contains invalid values. All values in the search vector must \
                     be a 32-bit floating-point number attribute",
                )
            })?;

        let accounts = self.state.read();
        let empty_ddb = crate::state::DynamoDbState::new(&req.account_id, &req.region);
        let state = accounts.get(&req.account_id).unwrap_or(&empty_ddb);
        let table = get_table(&state.tables, table_name)?;
        let not_served = || {
            validation(format!(
                "The table does not have the specified index: {index_name}"
            ))
        };
        let index = table
            .vector_indexes
            .iter()
            .find(|i| i.index_name == index_name)
            .ok_or_else(not_served)?;
        match index.phase(Utc::now()) {
            VectorIndexPhase::Allocating => return Err(not_served()),
            VectorIndexPhase::Backfilling => {
                return Err(validation(format!(
                    "Cannot search backfilling vector index: {index_name}"
                )))
            }
            VectorIndexPhase::Active => {}
        }
        if query_vector.len() as i64 != index.dimensions {
            return Err(validation(format!(
                "Input search vector dimension {} does not match vector index dimension {}",
                query_vector.len(),
                index.dimensions
            )));
        }

        let condition = body["SearchConditionExpression"]
            .as_str()
            .filter(|s| !s.trim().is_empty());
        let has_hash = index.search_schema.iter().any(|(_, kind)| kind == "HASH");
        let terms = match condition {
            Some(expr) => parse_search_condition(
                expr,
                index,
                &parse_expression_attribute_names(&body),
                &parse_expression_attribute_values(&body),
            )?,
            None => Vec::new(),
        };
        // Every search on an index with a HASH element is scoped to one
        // partition of it.
        let hash_bound = index
            .search_schema
            .iter()
            .filter(|(_, kind)| kind == "HASH")
            .all(|(attr, _)| terms.iter().any(|t| t.attribute == *attr));
        if has_hash && !hash_bound {
            return Err(validation(
                "SearchConditionExpression must be provided when SearchSchema has a HASH key",
            ));
        }

        let mut scored: Vec<(f32, Item)> = table
            .items
            .iter()
            .filter_map(|item| {
                let entry = vector_entry(table, index, item)?;
                let matches = terms.iter().all(|t| {
                    entry.get(&t.attribute).is_some_and(|v| {
                        compare_attribute_values(Some(v), Some(&t.value))
                            == std::cmp::Ordering::Equal
                            && type_tag(v) == type_tag(&t.value)
                    })
                });
                if !matches {
                    return None;
                }
                let candidate = indexed_vector(index, item)?;
                Some((
                    score(&index.distance_function, &query_vector, &candidate),
                    entry,
                ))
            })
            .collect();
        if index.distance_function == "DOT_PRODUCT" {
            scored.sort_by(|a, b| b.0.total_cmp(&a.0));
        } else {
            scored.sort_by(|a, b| a.0.total_cmp(&b.0));
        }
        scored.truncate(top_k as usize);

        let projects = body["ProjectionExpression"]
            .as_str()
            .is_some_and(|p| !p.is_empty());
        let mut returned_bytes = 0usize;
        let results: Vec<Value> = scored
            .into_iter()
            .map(|(s, mut entry)| {
                returned_bytes += entry_bytes(index, &entry);
                // The vector itself comes back only when projected for.
                let item = if projects {
                    project_item(&entry, &body)
                } else {
                    entry.remove(&index.vector_attribute);
                    entry
                };
                json!({ "Item": item, "Score": f64::from(s) })
            })
            .collect();

        let mut out = json!({ "SearchResults": results });
        if return_consumed_mode(&body) != "NONE" {
            // Metered in bytes processed: the query vector plus the entries
            // read back, held to a floor. Reported bare, with no classic
            // capacity fields, identically under TOTAL and INDEXES.
            let bytes = (4 * query_vector.len() + returned_bytes) as f64;
            out["ConsumedCapacity"] =
                json!({ "VectorSearchRequestBytes": bytes.max(VECTOR_SEARCH_FLOOR_BYTES) });
        }
        Self::ok_json(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn number_bytes_matches_the_measured_sizes() {
        for (literal, bytes) in [
            ("0", 1),
            ("1", 2),
            ("12", 2),
            ("123", 3),
            ("1234", 3),
            ("0042", 2),
            ("100", 2),
            ("1010", 3),
            ("0.0000001", 2),
            ("1E125", 2),
            ("1.5", 3),
            ("3.14159", 5),
            ("123456", 4),
            ("100.5", 4),
            ("0.15", 2),
            ("-42", 3),
            ("-0", 1),
        ] {
            assert_eq!(number_bytes(literal), bytes, "{literal}");
        }
    }

    #[test]
    fn scores_follow_each_distance_function() {
        let q = [1.0f32, 0.0, 0.0];
        assert_eq!(score("COSINE", &q, &[1.0, 0.0, 0.0]), 0.0);
        assert_eq!(score("COSINE", &q, &[0.0, 1.0, 0.0]), 1.0);
        assert_eq!(score("COSINE", &q, &[-1.0, 0.0, 0.0]), 2.0);
        assert_eq!(score("EUCLIDEAN", &q, &[-1.0, 0.0, 0.0]), 2.0);
        assert_eq!(score("DOT_PRODUCT", &q, &[-1.0, 0.0, 0.0]), -1.0);
    }

    #[test]
    fn search_conditions_accept_only_equality() {
        let index = VectorIndex {
            index_name: "vix".into(),
            index_arn: String::new(),
            vector_attribute: "embedding".into(),
            dimensions: 3,
            distance_function: "COSINE".into(),
            search_schema: vec![
                ("tenant".into(), "HASH".into()),
                ("category".into(), "INLINE_FILTER".into()),
            ],
            projection: crate::state::Projection {
                projection_type: "ALL".into(),
                non_key_attributes: vec![],
            },
            online_created_at: None,
        };
        let values: HashMap<String, Value> = [
            (":t".to_string(), json!({"S": "t1"})),
            (":c".to_string(), json!({"S": "c1"})),
        ]
        .into();
        let names = HashMap::new();
        let ok = parse_search_condition("tenant = :t AND category = :c", &index, &names, &values)
            .unwrap();
        assert_eq!(ok.len(), 2);
        for bad in [
            "tenant < :t",
            "tenant = :t AND category >= :c",
            "tenant <> :t",
        ] {
            let err = parse_search_condition(bad, &index, &names, &values)
                .err()
                .unwrap();
            assert!(err.to_string().contains("Invalid comparator"), "{bad}");
        }
        let err = parse_search_condition("other = :t", &index, &names, &values)
            .err()
            .unwrap();
        assert!(err.to_string().contains("Invalid attribute: other"));
    }
}
