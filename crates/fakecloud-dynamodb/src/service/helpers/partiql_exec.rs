//! PartiQL for DynamoDB: running a parsed [`Statement`] against the tables.
//!
//! The semantics follow DynamoDB rather than general PartiQL:
//!
//! * UPDATE and DELETE act on the one item their WHERE clause pins by an
//!   equality on every primary-key attribute. Any other conjunct is a
//!   condition on that item: false fails the write with
//!   ConditionalCheckFailedException. An UPDATE of a missing item fails the
//!   same way (it is not an upsert); a DELETE of one is a no-op.
//! * A SELECT is a point read when it pins the whole key, a query when it
//!   pins the partition key, and a scan otherwise. `Limit` bounds the rows
//!   evaluated, not the rows returned, and `NextToken` resumes after the last
//!   evaluated row by its position in the read order, so a page survives rows
//!   deleted in between.
//! * `FROM "table"."index"` reads the index: its rows are the items carrying
//!   the index key, holding only what the index projects. A GSI refuses a
//!   column it does not project; an LSI fetches it from the table. A read
//!   keyed on the index partition key refuses a filter on an unprojected
//!   attribute; an unkeyed one just finds no such attribute.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};

use base64::Engine;
use http::StatusCode;
use serde_json::{json, Value};

use fakecloud_core::service::AwsServiceError;

use super::partiql_parse::{
    conjuncts, equality_on, expr_attributes, key_equality, key_values, path_root, validation,
    ArithOp, CmpOp, Expr, OrderBy, Path, PathSegment, Projection, Returning, Source, Statement,
    UpdateOp,
};
use super::{
    build_capacity, check_put_item_size, check_update_item_size, item_size, item_write_consumed,
    normalize_item_numbers, normalize_value_numbers, read_units, CapacitySplit, Consumed,
};
use crate::state::{
    attribute_type_and_value, AttributeValue, DynamoTable, Projection as IndexProjection, RowKey,
};

type Item = HashMap<String, AttributeValue>;

/// The data one page of a read evaluates at most.
const MAX_PAGE_BYTES: usize = 1024 * 1024;

/// Which API is running the statement: each has its own rules about what a
/// statement may do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Surface {
    Execute,
    Batch,
    Transaction,
}

#[derive(Debug, Clone)]
pub(crate) struct ExecOptions<'a> {
    pub surface: Surface,
    pub consistent_read: bool,
    pub limit: Option<usize>,
    pub next_token: Option<&'a str>,
    /// `ReturnValuesOnConditionCheckFailure: ALL_OLD`.
    pub return_old_on_condition_failure: bool,
}

impl ExecOptions<'_> {
    pub(crate) fn new(surface: Surface) -> Self {
        Self {
            surface,
            consistent_read: false,
            limit: None,
            next_token: None,
            return_old_on_condition_failure: false,
        }
    }
}

/// Capacity one statement consumed, by arm, and whether it was a read.
#[derive(Debug, Clone, Default)]
pub(crate) struct Capacity {
    pub read: bool,
    pub consumed: Consumed,
}

impl Capacity {
    fn read(consumed: Consumed) -> Self {
        Capacity {
            read: true,
            consumed,
        }
    }

    fn write(consumed: Consumed) -> Self {
        Capacity {
            read: false,
            consumed,
        }
    }

    /// The `ConsumedCapacity` block for this capacity. `split` adds the
    /// read/write breakdown the transactional APIs report.
    pub(crate) fn to_json(&self, mode: &str, table_name: &str, split: bool) -> Value {
        let empty = Consumed::default();
        if self.read {
            capacity_json(mode, table_name, &self.consumed, &empty, split)
        } else {
            capacity_json(mode, table_name, &empty, &self.consumed, split)
        }
    }
}

/// The `ConsumedCapacity` block for a table's read and write units. A table
/// only read or only written is reported by the shared builder; one both
/// read and written (a transaction's condition check beside its write) sums
/// the two in every arm, and `split` reports each as what it was.
pub(crate) fn capacity_json(
    mode: &str,
    table_name: &str,
    reads: &Consumed,
    writes: &Consumed,
    split: bool,
) -> Value {
    let split_as = |kind| if split { kind } else { CapacitySplit::None };
    if writes.total() == 0.0 {
        return build_capacity(mode, table_name, reads, split_as(CapacitySplit::Read));
    }
    if reads.total() == 0.0 {
        return build_capacity(mode, table_name, writes, split_as(CapacitySplit::Write));
    }
    let mut both = reads.clone();
    both.add(writes);
    let mut cc = build_capacity(mode, table_name, &both, CapacitySplit::None);
    if split {
        let mark = |arm: &mut Value, r: f64, w: f64| {
            if r > 0.0 {
                arm["ReadCapacityUnits"] = json!(r);
            }
            if w > 0.0 {
                arm["WriteCapacityUnits"] = json!(w);
            }
        };
        mark(&mut cc, reads.total(), writes.total());
        if let Some(arm) = cc.get_mut("Table") {
            mark(arm, reads.table, writes.table);
        }
        for (group, r, w) in [
            ("GlobalSecondaryIndexes", &reads.gsi, &writes.gsi),
            ("LocalSecondaryIndexes", &reads.lsi, &writes.lsi),
        ] {
            if let Some(map) = cc.get_mut(group).and_then(Value::as_object_mut) {
                for (name, arm) in map.iter_mut() {
                    let get = |m: &BTreeMap<String, f64>| m.get(name).copied().unwrap_or(0.0);
                    mark(arm, get(r), get(w));
                }
            }
        }
    }
    cc
}

/// A write a statement made, for the stream and Kinesis hooks.
#[derive(Debug, Clone)]
pub(crate) struct Change {
    pub event_name: &'static str,
    pub keys: Item,
    pub old_image: Option<Item>,
    pub new_image: Option<Item>,
}

#[derive(Debug, Clone)]
pub(crate) struct Outcome {
    /// The table's own name (an ARN in the statement resolves to it).
    pub table_name: String,
    /// The rows the statement returns (a SELECT's page, or a RETURNING row).
    pub items: Vec<Item>,
    /// Whether the statement returns rows at all (a SELECT, or a write with
    /// RETURNING); a plain write returns no `Items`.
    pub returns_items: bool,
    pub next_token: Option<String>,
    pub capacity: Capacity,
    pub change: Option<Change>,
}

/// A statement that failed. `table` names the table when the statement got as
/// far as running against it (a condition failure, a duplicate insert), which
/// BatchExecuteStatement echoes; a statement rejected before it ran has none.
#[derive(Debug)]
pub(crate) struct ExecError {
    pub error: Box<AwsServiceError>,
    pub table: Option<String>,
}

impl ExecError {
    /// A failure of a statement that ran against `table`.
    fn ran(error: AwsServiceError, table: &str) -> Self {
        ExecError {
            error: Box::new(error),
            table: Some(table.to_string()),
        }
    }
}

impl From<AwsServiceError> for ExecError {
    fn from(error: AwsServiceError) -> Self {
        ExecError {
            error: Box::new(error),
            table: None,
        }
    }
}

fn condition_failed(old: Option<&Item>, return_old: bool) -> AwsServiceError {
    let mut fields = Vec::new();
    if return_old {
        if let Some(item) = old {
            if let Ok(s) = serde_json::to_string(item) {
                fields.push(("Item".to_string(), s));
            }
        }
    }
    AwsServiceError::aws_error_with_fields(
        StatusCode::BAD_REQUEST,
        "ConditionalCheckFailedException",
        "The conditional request failed",
        fields,
    )
}

/// Statement-level rules that hold whatever the table: DELETE returns only
/// `ALL OLD *`.
pub(crate) fn check_statement(stmt: &Statement) -> Result<(), AwsServiceError> {
    if let Statement::Delete {
        returning: Some(r), ..
    } = stmt
    {
        if !(r.all && !r.new) {
            return Err(validation(format!(
                "Invalid returning clause: {}. Only RETURNING ALL OLD * is allowed in DELETE statements.",
                r.text()
            )));
        }
    }
    Ok(())
}

/// Run one statement.
pub(crate) fn execute(
    tables: &mut BTreeMap<String, DynamoTable>,
    stmt: &Statement,
    opts: &ExecOptions<'_>,
) -> Result<Outcome, ExecError> {
    check_statement(stmt)?;
    match stmt {
        Statement::Select {
            projection,
            source,
            filter,
            order_by,
        } => {
            let table = super::get_table(tables, &source.table)?;
            select(table, projection, source, filter.as_ref(), order_by, opts)
        }
        Statement::Exists(inner) => {
            let Statement::Select { source, filter, .. } = inner.as_ref() else {
                return Err(validation("EXISTS requires a SELECT").into());
            };
            let table = super::get_table(tables, &source.table)?;
            let (key, condition) = pinned_key(table, filter.as_ref())?;
            let found = find(table, &key).map(|(_, item)| item);
            if !found.is_some_and(|item| condition.iter().all(|c| test(c, item))) {
                return Err(ExecError::ran(condition_failed(None, false), &table.name));
            }
            Ok(Outcome {
                table_name: table.name.clone(),
                items: Vec::new(),
                returns_items: false,
                next_token: None,
                capacity: Capacity::read(Consumed::table(read_units(
                    found.map_or(0, item_size),
                    false,
                ))),
                change: None,
            })
        }
        Statement::Insert { table, value } => insert(tables, table, value),
        Statement::Update {
            source,
            ops,
            filter,
            returning,
        } => update(tables, source, ops, filter.as_ref(), *returning, opts),
        Statement::Delete {
            source,
            filter,
            returning,
        } => delete(tables, source, filter.as_ref(), *returning, opts),
    }
}

// --- Evaluation ---

/// Resolve a document path in an item.
pub(crate) fn resolve<'a>(item: &'a Item, path: &[PathSegment]) -> Option<&'a Value> {
    let mut cur: Option<&Value> = None;
    for (i, seg) in path.iter().enumerate() {
        cur = match (i, seg) {
            (0, PathSegment::Name(n)) => item.get(n),
            (_, PathSegment::Name(n)) => cur?.get("M")?.get(n),
            (_, PathSegment::Index(ix)) => cur?.get("L")?.get(*ix),
        };
        cur?;
    }
    cur
}

fn set_members(v: &Value) -> Option<(&str, &Vec<Value>)> {
    match attribute_type_and_value(v) {
        Some((t @ ("SS" | "NS" | "BS"), Value::Array(a))) => Some((t, a)),
        _ => None,
    }
}

/// DynamoDB equality: same type and same value, numbers by value, sets as
/// unordered collections, maps by content and lists element by element.
pub(crate) fn av_equal(a: &Value, b: &Value) -> bool {
    let (Some((ta, va)), Some((tb, vb))) =
        (attribute_type_and_value(a), attribute_type_and_value(b))
    else {
        return a == b;
    };
    if ta != tb {
        return false;
    }
    match ta {
        "N" => super::values_equal(Some(a), Some(b)),
        "SS" | "BS" | "NS" => {
            let (Some(xa), Some(xb)) = (va.as_array(), vb.as_array()) else {
                return false;
            };
            let same = |x: &Value, y: &Value| {
                if ta == "NS" {
                    super::ns_members_equal(x, y)
                } else {
                    x == y
                }
            };
            xa.len() == xb.len()
                && xa.iter().all(|x| xb.iter().any(|y| same(x, y)))
                && xb.iter().all(|y| xa.iter().any(|x| same(x, y)))
        }
        "L" => match (va.as_array(), vb.as_array()) {
            (Some(xa), Some(xb)) => {
                xa.len() == xb.len() && xa.iter().zip(xb).all(|(x, y)| av_equal(x, y))
            }
            _ => false,
        },
        "M" => match (va.as_object(), vb.as_object()) {
            (Some(xa), Some(xb)) => {
                xa.len() == xb.len()
                    && xa
                        .iter()
                        .all(|(k, x)| xb.get(k).is_some_and(|y| av_equal(x, y)))
            }
            _ => false,
        },
        _ => va == vb,
    }
}

/// Order two values of one scalar type (S, N or B); `None` for anything else.
fn av_order(a: &Value, b: &Value) -> Option<std::cmp::Ordering> {
    super::comparable_types(Some(a), Some(b)).then(|| {
        if a.get("N").is_some() {
            let (x, y) = (a["N"].as_str()?, b["N"].as_str()?);
            if !super::is_valid_number(x) || !super::is_valid_number(y) {
                return None;
            }
        }
        Some(super::compare_attribute_values(Some(a), Some(b)))
    })?
}

/// Evaluate an expression to a value; `None` is MISSING.
fn eval(e: &Expr, item: &Item) -> Option<Value> {
    match e {
        Expr::Path(p) => resolve(item, p).cloned(),
        Expr::Lit(v) => Some(v.clone()),
        Expr::Missing => None,
        // A constructor holding a MISSING element has no value: it is never
        // built with the element silently dropped.
        Expr::List(items) => Some(json!({
            "L": items.iter().map(|i| eval(i, item)).collect::<Option<Vec<_>>>()?
        })),
        Expr::Tuple(fields) => Some(json!({
            "M": fields
                .iter()
                .map(|(k, v)| eval(v, item).map(|v| (k.clone(), v)))
                .collect::<Option<serde_json::Map<_, _>>>()?
        })),
        Expr::Bag(items) => {
            let values = items
                .iter()
                .map(|i| eval(i, item))
                .collect::<Option<Vec<_>>>()?;
            // A set holds scalars of one type: S, N or B.
            let kind = match attribute_type_and_value(values.first()?)? {
                (k @ ("S" | "N" | "B"), _) => k,
                _ => return None,
            };
            let members = values
                .iter()
                .map(|v| match attribute_type_and_value(v) {
                    Some((k, m)) if k == kind => Some(m.clone()),
                    _ => None,
                })
                .collect::<Option<Vec<_>>>()?;
            Some(json!({ format!("{kind}S"): members }))
        }
        Expr::Arith(l, op, r) => {
            let (l, r) = (eval(l, item)?, eval(r, item)?);
            let sum =
                super::decimal_add_sub(l["N"].as_str()?, r["N"].as_str()?, *op == ArithOp::Add)?;
            Some(json!({ "N": sum }))
        }
        Expr::Neg(inner) => {
            let v = eval(inner, item)?;
            let n = super::decimal_add_sub("0", v["N"].as_str()?, false)?;
            Some(json!({ "N": n }))
        }
        Expr::Func(name, args) if name == "size" => {
            let v = eval(args.first()?, item)?;
            Some(json!({ "N": super::attribute_size(&v)?.to_string() }))
        }
        Expr::Func(name, _) if name != "size" && !is_predicate_function(name) => None,
        // A predicate used as an operand (`begins_with(a, 'x') = TRUE`).
        other => Some(json!({ "BOOL": test(other, item) })),
    }
}

fn is_predicate_function(name: &str) -> bool {
    matches!(
        name,
        "begins_with" | "contains" | "attribute_type" | "attribute_exists" | "attribute_not_exists"
    )
}

/// Evaluate an expression as a WHERE predicate.
pub(crate) fn test(e: &Expr, item: &Item) -> bool {
    match e {
        Expr::And(l, r) => test(l, item) && test(r, item),
        Expr::Or(l, r) => test(l, item) || test(r, item),
        Expr::Not(inner) => !test(inner, item),
        Expr::Cmp(l, op, r) => {
            let (l, r) = (eval(l, item), eval(r, item));
            match op {
                CmpOp::Eq => matches!((&l, &r), (Some(a), Some(b)) if av_equal(a, b)),
                CmpOp::Ne => !matches!((&l, &r), (Some(a), Some(b)) if av_equal(a, b)),
                _ => {
                    let (Some(a), Some(b)) = (l, r) else {
                        return false;
                    };
                    av_order(&a, &b).is_some_and(|o| match op {
                        CmpOp::Lt => o.is_lt(),
                        CmpOp::Le => o.is_le(),
                        CmpOp::Gt => o.is_gt(),
                        _ => o.is_ge(),
                    })
                }
            }
        }
        Expr::Between(v, lo, hi) => {
            let (Some(v), Some(lo), Some(hi)) = (eval(v, item), eval(lo, item), eval(hi, item))
            else {
                return false;
            };
            av_order(&v, &lo).is_some_and(|o| o.is_ge())
                && av_order(&v, &hi).is_some_and(|o| o.is_le())
        }
        Expr::In(v, items) => eval(v, item).is_some_and(|v| {
            items
                .iter()
                .filter_map(|i| eval(i, item))
                .any(|i| av_equal(&v, &i))
        }),
        Expr::Like(v, pattern) => match (eval(v, item), eval(pattern, item)) {
            (Some(v), Some(p)) => match (v["S"].as_str(), p["S"].as_str()) {
                (Some(s), Some(p)) => super::match_like(s, p),
                _ => false,
            },
            _ => false,
        },
        Expr::Is {
            expr,
            negated,
            null,
        } => {
            let v = eval(expr, item);
            let holds = if *null {
                v.as_ref().is_none_or(|v| v.get("NULL").is_some())
            } else {
                v.is_none()
            };
            holds != *negated
        }
        Expr::Func(name, args) => test_function(name, args, item),
        Expr::Path(_) | Expr::Lit(_) => {
            eval(e, item).is_some_and(|v| v.get("BOOL") == Some(&Value::Bool(true)))
        }
        _ => false,
    }
}

fn test_function(name: &str, args: &[Expr], item: &Item) -> bool {
    let arg = |i: usize| args.get(i).and_then(|a| eval(a, item));
    match name {
        "begins_with" => match (arg(0), arg(1)) {
            (Some(v), Some(p)) => super::attribute_begins_with(&v, &p),
            _ => false,
        },
        "contains" => match (arg(0), arg(1)) {
            (Some(v), Some(needle)) => {
                if let (Some(s), Some(n)) = (v["S"].as_str(), needle["S"].as_str()) {
                    s.contains(n)
                } else if let Some((kind, members)) = set_members(&v) {
                    let elem = &kind[..1];
                    needle.get(elem).is_some_and(|n| {
                        members.iter().any(|m| {
                            if elem == "N" {
                                super::ns_members_equal(m, n)
                            } else {
                                m == n
                            }
                        })
                    })
                } else if let Some(list) = v["L"].as_array() {
                    list.iter().any(|e| av_equal(e, &needle))
                } else {
                    false
                }
            }
            _ => false,
        },
        "attribute_type" => match (arg(0), arg(1)) {
            (Some(v), Some(t)) => attribute_type_and_value(&v)
                .zip(t["S"].as_str())
                .is_some_and(|((actual, _), want)| actual == want),
            _ => false,
        },
        "attribute_exists" | "exists" => arg(0).is_some(),
        "attribute_not_exists" => arg(0).is_none(),
        _ => false,
    }
}

// --- Keys ---

/// The item a write's WHERE clause pins: an equality on every primary-key
/// attribute among its top-level conjuncts. The rest of the clause is the
/// condition the item must meet.
fn pinned_key<'e>(
    table: &DynamoTable,
    filter: Option<&'e Expr>,
) -> Result<(Item, Vec<&'e Expr>), AwsServiceError> {
    let parts = filter.map(conjuncts).unwrap_or_default();
    let mut key = Item::new();
    for name in std::iter::once(table.hash_key_name()).chain(table.range_key_name()) {
        let value = key_equality(&parts, name).cloned().ok_or_else(|| {
            validation("Where clause does not contain a mandatory equality on all key attributes")
        })?;
        key.insert(name.to_string(), value);
    }
    super::validate_key_attributes_in_key(table, &key)?;
    Ok((key, parts))
}

fn find<'t>(table: &'t DynamoTable, key: &Item) -> Option<(crate::state::ItemId, &'t Item)> {
    let id = table.find_item_index(key)?;
    Some((id, table.items.get(id)?))
}

// --- SELECT ---

/// A resolved index: its key attributes and what it projects.
struct IndexInfo<'t> {
    name: &'t str,
    global: bool,
    hash: &'t str,
    range: Option<&'t str>,
    projection: &'t IndexProjection,
}

impl IndexInfo<'_> {
    fn projects(&self, table: &DynamoTable, attr: &str) -> bool {
        self.projection.projection_type == "ALL"
            || attr == table.hash_key_name()
            || Some(attr) == table.range_key_name()
            || attr == self.hash
            || Some(attr) == self.range
            || (self.projection.projection_type == "INCLUDE"
                && self.projection.non_key_attributes.iter().any(|a| a == attr))
    }
}

fn resolve_index<'t>(table: &'t DynamoTable, name: &str) -> Result<IndexInfo<'t>, AwsServiceError> {
    let key_of = |schema: &'t [crate::state::KeySchemaElement], kind: &str| {
        schema
            .iter()
            .find(|k| k.key_type == kind)
            .map(|k| k.attribute_name.as_str())
    };
    if let Some(g) = table.gsi.iter().find(|g| g.index_name == name) {
        return Ok(IndexInfo {
            name: &g.index_name,
            global: true,
            hash: key_of(&g.key_schema, "HASH").unwrap_or_default(),
            range: key_of(&g.key_schema, "RANGE"),
            projection: &g.projection,
        });
    }
    if let Some(l) = table.lsi.iter().find(|l| l.index_name == name) {
        return Ok(IndexInfo {
            name: &l.index_name,
            global: false,
            hash: table.hash_key_name(),
            range: key_of(&l.key_schema, "RANGE"),
            projection: &l.projection,
        });
    }
    if table.vector_indexes.iter().any(|v| v.index_name == name) {
        return Err(validation(
            "Scan operation not supported on this index type",
        ));
    }
    Err(validation("The table does not have the specified index"))
}

/// The pagination cursor: which read minted it and the row it stopped at.
fn encode_token(table: &str, index: Option<&str>, after: &Item) -> String {
    base64::engine::general_purpose::STANDARD
        .encode(json!({ "Table": table, "Index": index, "After": after }).to_string())
}

fn decode_token(token: &str, table: &str, index: Option<&str>) -> Result<Item, AwsServiceError> {
    let invalid = || validation("The provided starting key is invalid");
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(token)
        .map_err(|_| invalid())?;
    let v: Value = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
    if v["Table"].as_str() != Some(table) || v["Index"].as_str() != index {
        return Err(invalid());
    }
    serde_json::from_value(v["After"].clone()).map_err(|_| invalid())
}

/// One row a read walks.
struct Row<'t> {
    base: &'t Item,
    view: Cow<'t, Item>,
    key: Option<RowKey>,
}

fn select<'t>(
    table: &'t DynamoTable,
    projection: &Projection,
    source: &Source,
    filter: Option<&Expr>,
    order_by: &[OrderBy],
    opts: &ExecOptions<'_>,
) -> Result<Outcome, ExecError> {
    let index = source
        .index
        .as_deref()
        .map(|name| resolve_index(table, name))
        .transpose()?;
    let parts = filter.map(conjuncts).unwrap_or_default();

    if opts.surface == Surface::Batch {
        let pins_key = index.is_none()
            && std::iter::once(table.hash_key_name())
                .chain(table.range_key_name())
                .all(|k| {
                    parts
                        .iter()
                        .any(|c| equality_on(c).is_some_and(|(a, _)| a == k))
                });
        if !pins_key {
            return Err(validation(
                "Select statements within BatchExecuteStatement must specify the primary key in the where clause.",
            )
            .into());
        }
    }
    if let Some(ix) = &index {
        if opts.surface == Surface::Transaction {
            return Err(
                validation("Reads on indices are not supported within transactions.").into(),
            );
        }
        if ix.global && opts.consistent_read {
            return Err(validation(
                "Strongly consistent read is not supported on Global Secondary Indexes",
            )
            .into());
        }
        if ix.global {
            if let Projection::Paths(paths) = projection {
                let mut missing: Vec<&str> = Vec::new();
                for p in paths {
                    let root = path_root(p);
                    if !ix.projects(table, root) && !missing.contains(&root) {
                        missing.push(root);
                    }
                }
                if !missing.is_empty() {
                    return Err(validation(format!(
                        "One or more parameter values were invalid: Global secondary index {} does not project [{}]",
                        ix.name,
                        missing.join(", ")
                    ))
                    .into());
                }
            }
        }
        if key_values(&parts, ix.hash).is_some() {
            let mut attrs = Vec::new();
            if let Some(f) = filter {
                expr_attributes(f, &mut attrs);
            }
            attrs.retain(|a| !ix.projects(table, a));
            if !attrs.is_empty() {
                return Err(validation(format!(
                    "One or more parameter values were invalid: Secondary index {} does not project one or more filter attributes: [{}]",
                    ix.name,
                    attrs.join(", ")
                ))
                .into());
            }
        }
    }

    let after = opts
        .next_token
        .map(|t| decode_token(t, &table.name, index.as_ref().map(|i| i.name)))
        .transpose()?;

    // The rows the read walks: each base item, the row as the read sees it
    // (an index row holds only what the index projects), and its primary key.
    let hash_attr = index.as_ref().map_or(table.hash_key_name(), |i| i.hash);
    let pinned = key_values(&parts, hash_attr);
    // A point read: the whole primary key pinned to one value. The key is
    // built from the same conjuncts that make it a point read.
    let point_key: Option<Item> = match (&index, &pinned) {
        (None, Some(values)) if values.len() == 1 => {
            let mut key = Item::new();
            key.insert(hash_attr.to_string(), values[0].clone());
            match table.range_key_name() {
                Some(rk) => key_equality(&parts, rk).map(|v| {
                    key.insert(rk.to_string(), v.clone());
                    key
                }),
                None => Some(key),
            }
        }
        _ => None,
    };
    let row_of = |item: &'t Item| {
        let view = match &index {
            Some(ix) => Cow::Owned(crate::service::queries::apply_index_projection(
                item.clone(),
                ix.projection,
                &std::iter::once(ix.hash.to_string())
                    .chain(ix.range.map(str::to_string))
                    .collect::<Vec<_>>(),
                table.hash_key_name(),
                table.range_key_name(),
            )),
            None => Cow::Borrowed(item),
        };
        Row {
            base: item,
            view,
            key: table.encode_key_with(|n| item.get(n)),
        }
    };
    let mut rows: Vec<Row<'t>> = if let Some(key) = &point_key {
        find(table, key)
            .map(|(_, item)| vec![row_of(item)])
            .unwrap_or_default()
    } else {
        table
            .items
            .iter()
            .filter(|item| match &index {
                Some(ix) => {
                    item.contains_key(ix.hash) && ix.range.is_none_or(|r| item.contains_key(r))
                }
                None => true,
            })
            .filter(|item| match &pinned {
                Some(values) => item
                    .get(hash_attr)
                    .is_some_and(|v| values.iter().any(|p| av_equal(v, p))),
                None => true,
            })
            .map(row_of)
            .collect()
    };

    // The read order: ORDER BY, then the index key, then the primary key in
    // Scan order.
    let order = |a: &Item, ka: &Option<RowKey>, b: &Item, kb: &Option<RowKey>| {
        for ob in order_by {
            let o = super::compare_attribute_values(resolve(a, &ob.path), resolve(b, &ob.path));
            let o = if ob.descending { o.reverse() } else { o };
            if o.is_ne() {
                return o;
            }
        }
        if let Some(ix) = &index {
            let o =
                super::compare_attribute_values(a.get(ix.hash), b.get(ix.hash)).then_with(|| {
                    ix.range.map_or(std::cmp::Ordering::Equal, |r| {
                        super::compare_attribute_values(a.get(r), b.get(r))
                    })
                });
            if o.is_ne() {
                return o;
            }
        }
        (ka.is_none(), ka).cmp(&(kb.is_none(), kb))
    };
    rows.sort_by(|a, b| order(&a.view, &a.key, &b.view, &b.key));
    if let Some(after) = &after {
        let after_key = table.encode_key_with(|n| after.get(n));
        rows.retain(|r| order(&r.view, &r.key, after, &after_key).is_gt());
    }
    // A page ends at `Limit` rows evaluated or once 1 MB of them has been
    // read, whichever comes first.
    let mut page = opts.limit.unwrap_or(usize::MAX).min(rows.len());
    let mut bytes = 0;
    for (i, row) in rows.iter().enumerate().take(page) {
        bytes += item_size(&row.view);
        if bytes >= MAX_PAGE_BYTES {
            page = i + 1;
            break;
        }
    }
    let more = rows.len() > page;
    rows.truncate(page);
    let next_token = if more {
        rows.last().map(|last| {
            let row = &last.view;
            let mut cursor = Item::new();
            let mut keep = |name: &str| {
                if let Some(v) = row.get(name) {
                    cursor.insert(name.to_string(), v.clone());
                }
            };
            keep(table.hash_key_name());
            if let Some(r) = table.range_key_name() {
                keep(r);
            }
            if let Some(ix) = &index {
                keep(ix.hash);
                if let Some(r) = ix.range {
                    keep(r);
                }
            }
            for ob in order_by {
                keep(path_root(&ob.path));
            }
            encode_token(&table.name, index.as_ref().map(|i| i.name), &cursor)
        })
    } else {
        None
    };

    // An LSI serves a column it does not project from the table.
    let reach_back = match (&index, projection) {
        (Some(ix), Projection::Paths(paths)) if !ix.global => {
            paths.iter().any(|p| !ix.projects(table, path_root(p)))
        }
        _ => false,
    };

    let mut consumed = Consumed::default();
    let walked_bytes: usize = rows.iter().map(|r| item_size(&r.view)).sum();
    match &index {
        None => consumed.table = read_units(walked_bytes, opts.consistent_read),
        Some(ix) => {
            let units = read_units(walked_bytes, opts.consistent_read);
            if ix.global {
                consumed.gsi.insert(ix.name.to_string(), units);
            } else {
                consumed.lsi.insert(ix.name.to_string(), units);
            }
            if reach_back {
                consumed.table = rows
                    .iter()
                    .map(|r| read_units(item_size(r.base), opts.consistent_read))
                    .sum();
            }
        }
    }

    let items: Vec<Item> = rows
        .iter()
        .filter(|r| filter.is_none_or(|f| test(f, &r.view)))
        .map(|r| match projection {
            Projection::Star => r.view.clone().into_owned(),
            Projection::Paths(paths) => {
                project_columns(if reach_back { r.base } else { &r.view }, paths)
            }
        })
        .collect();

    Ok(Outcome {
        table_name: table.name.clone(),
        items,
        returns_items: true,
        next_token,
        capacity: Capacity::read(consumed),
        change: None,
    })
}

/// A SELECT column list: each path's value under the name of its last step
/// (`SELECT a.b` returns `b`).
fn project_columns(item: &Item, paths: &[Path]) -> Item {
    let mut out = Item::new();
    for (i, path) in paths.iter().enumerate() {
        if let Some(v) = resolve(item, path) {
            let name = match path.last() {
                Some(PathSegment::Name(n)) => n.clone(),
                _ => format!("_{}", i + 1),
            };
            out.insert(name, v.clone());
        }
    }
    out
}

// --- INSERT ---

/// The item an INSERT writes: its VALUE, a tuple or a bound map parameter.
/// IAM reads the item from here too, so it sees exactly what is written.
pub(crate) fn insert_item(value: &Expr) -> Result<Item, AwsServiceError> {
    match eval(value, &Item::new()) {
        Some(v) if v.get("M").is_some_and(Value::is_object) => {
            serde_json::from_value(v["M"].clone())
                .map_err(|_| validation("Unsupported value in INSERT"))
        }
        Some(_) => Err(validation(
            "Statement wasn't well formed, can't be processed: Expected a tuple in VALUE",
        )),
        None => Err(validation("Unsupported value in INSERT")),
    }
}

fn insert(
    tables: &mut BTreeMap<String, DynamoTable>,
    table_name: &str,
    value: &Expr,
) -> Result<Outcome, ExecError> {
    let table = super::get_table_mut(tables, table_name)?;
    let mut item = insert_item(value)?;
    super::validate_partiql_item_against_key_schema(table, &item)?;
    super::validate_item_attribute_values(&item)?;
    // Stored and measured exactly as PutItem stores and measures it.
    normalize_item_numbers(&mut item);
    check_put_item_size(&item)?;
    crate::service::vectors::validate_vector_item(
        &table.vector_indexes,
        &table.attribute_definitions,
        &item,
    )?;
    let key = super::extract_key(table, &item);
    if find(table, &key).is_some() {
        return Err(ExecError::ran(
            AwsServiceError::aws_error(
                StatusCode::BAD_REQUEST,
                "DuplicateItemException",
                "Duplicate primary key exists in table",
            ),
            &table.name,
        ));
    }
    let consumed = item_write_consumed(table, None, Some(&item));
    table.put_item_at_key(item.clone());
    Ok(Outcome {
        table_name: table.name.clone(),
        items: Vec::new(),
        returns_items: false,
        next_token: None,
        capacity: Capacity::write(consumed),
        change: Some(Change {
            event_name: "INSERT",
            keys: key,
            old_image: None,
            new_image: Some(item),
        }),
    })
}

// --- UPDATE / DELETE ---

/// A write names a table, never an index.
fn write_target<'t>(
    tables: &'t mut BTreeMap<String, DynamoTable>,
    source: &Source,
) -> Result<&'t mut DynamoTable, AwsServiceError> {
    let table = super::get_table_mut(tables, &source.table)?;
    if source.index.is_some() {
        return Err(validation("This operation is not supported on an index"));
    }
    Ok(table)
}

/// An UPDATE's SET/REMOVE list as an UpdateExpression, with the paths it
/// writes.
struct UpdatePlan {
    expression: String,
    names: HashMap<String, String>,
    values: HashMap<String, Value>,
    targets: Vec<String>,
}

fn plan_update(ops: &[UpdateOp], item: &Item) -> Result<UpdatePlan, AwsServiceError> {
    struct Builder<'i> {
        names: HashMap<String, String>,
        values: HashMap<String, Value>,
        item: &'i Item,
    }
    impl Builder<'_> {
        fn path(&mut self, p: &[PathSegment]) -> String {
            let mut out = String::new();
            for seg in p {
                match seg {
                    PathSegment::Name(n) => {
                        let placeholder = format!("#p{}", self.names.len());
                        if !out.is_empty() {
                            out.push('.');
                        }
                        out.push_str(&placeholder);
                        self.names.insert(placeholder, n.clone());
                    }
                    PathSegment::Index(i) => out.push_str(&format!("[{i}]")),
                }
            }
            out
        }

        fn value(&mut self, e: &Expr) -> Result<String, AwsServiceError> {
            if let Expr::Func(name, _) = e {
                return Err(validation(format!(
                    "Statement wasn't well formed, can't be processed: Unsupported function: {name}"
                )));
            }
            let v = eval(e, self.item).ok_or_else(|| {
                validation("An operand in the update expression has an incorrect data type")
            })?;
            let placeholder = format!(":v{}", self.values.len());
            self.values.insert(placeholder.clone(), v);
            Ok(placeholder)
        }

        fn operand(&mut self, e: &Expr) -> Result<String, AwsServiceError> {
            match e {
                Expr::Path(p) => Ok(self.path(p)),
                Expr::Func(name, args)
                    if matches!(name.as_str(), "list_append" | "if_not_exists")
                        && args.len() == 2 =>
                {
                    let a = self.operand(&args[0])?;
                    let b = self.operand(&args[1])?;
                    Ok(format!("{name}({a}, {b})"))
                }
                _ => self.value(e),
            }
        }
    }
    let mut b = Builder {
        names: HashMap::new(),
        values: HashMap::new(),
        item,
    };
    let (mut sets, mut removes, mut adds, mut deletes) = (vec![], vec![], vec![], vec![]);
    let mut targets = Vec::new();
    for op in ops {
        match op {
            UpdateOp::Set(path, rhs) => {
                let target = b.path(path);
                targets.push(target.clone());
                match rhs {
                    Expr::Func(name, args)
                        if matches!(name.as_str(), "set_add" | "set_delete")
                            && args.len() == 2
                            && matches!(&args[0], Expr::Path(p) if p == path) =>
                    {
                        let v = b.value(&args[1])?;
                        if name == "set_add" {
                            adds.push(format!("{target} {v}"));
                        } else {
                            deletes.push(format!("{target} {v}"));
                        }
                    }
                    Expr::Arith(l, op, r) => {
                        let l = b.operand(l)?;
                        let r = b.operand(r)?;
                        let sign = if *op == ArithOp::Add { '+' } else { '-' };
                        sets.push(format!("{target} = {l} {sign} {r}"));
                    }
                    _ => {
                        let v = b.operand(rhs)?;
                        sets.push(format!("{target} = {v}"));
                    }
                }
            }
            UpdateOp::Remove(path) => {
                let target = b.path(path);
                targets.push(target.clone());
                removes.push(target);
            }
        }
    }
    let mut expression = String::new();
    for (kw, parts) in [
        ("SET", &sets),
        ("REMOVE", &removes),
        ("ADD", &adds),
        ("DELETE", &deletes),
    ] {
        if !parts.is_empty() {
            expression.push_str(&format!("{kw} {} ", parts.join(", ")));
        }
    }
    Ok(UpdatePlan {
        expression: expression.trim_end().to_string(),
        names: b.names,
        values: b.values,
        targets,
    })
}

fn returned_row(returning: Returning, plan: &UpdatePlan, old: &Item, new: &Item) -> Option<Item> {
    let image = if returning.new { new } else { old };
    if returning.all {
        return Some(image.clone());
    }
    // MODIFIED projects each written path against the image: a path that no
    // longer (or never) resolves contributes nothing, and list elements pack
    // densely in index order.
    let body = json!({
        "ProjectionExpression": plan.targets.join(", "),
        "ExpressionAttributeNames": plan.names,
    });
    let row = super::project_item(image, &body);
    (!row.is_empty()).then_some(row)
}

fn update(
    tables: &mut BTreeMap<String, DynamoTable>,
    source: &Source,
    ops: &[UpdateOp],
    filter: Option<&Expr>,
    returning: Option<Returning>,
    opts: &ExecOptions<'_>,
) -> Result<Outcome, ExecError> {
    let table = write_target(tables, source)?;
    let (key, condition) = pinned_key(table, filter)?;
    let table_name = table.name.clone();
    let ran = |error: AwsServiceError| ExecError::ran(error, &table_name);
    let found = find(table, &key).map(|(id, item)| (id, item.clone()));
    // A malformed update is rejected whether or not the item exists.
    let mut plan = plan_update(ops, found.as_ref().map_or(&Item::new(), |(_, item)| item))?;
    // The values this update writes are validated and then stored in
    // canonical form, as UpdateItem does; the rest of the row is left as it
    // is.
    for v in plan.values.values() {
        super::validate_attribute_value(v)?;
    }
    for v in plan.values.values_mut() {
        normalize_value_numbers(v);
    }
    super::reject_key_attribute_update_expression(table, &plan.expression, &plan.names)?;
    let Some((id, old)) = found else {
        // Not an upsert: there is no item for the condition to hold on.
        return Err(ran(condition_failed(None, false)));
    };
    if !condition.iter().all(|c| test(c, &old)) {
        return Err(ran(condition_failed(
            Some(&old),
            opts.return_old_on_condition_failure,
        )));
    }
    let mut new = old.clone();
    super::apply_update_expression(&mut new, &plan.expression, &plan.names, &plan.values)?;
    // Measured flat, like a transacted Update: PartiQL does not carry
    // UpdateItem's per-clause charge.
    check_update_item_size(&new)?;
    crate::service::vectors::validate_vector_item(
        &table.vector_indexes,
        &table.attribute_definitions,
        &new,
    )?;
    let consumed = item_write_consumed(table, Some(&old), Some(&new));
    let stored = new.clone();
    table.mutate_item_at(id, |row| *row = stored);
    let items: Vec<Item> = returning
        .and_then(|r| returned_row(r, &plan, &old, &new))
        .into_iter()
        .collect();
    Ok(Outcome {
        table_name: table.name.clone(),
        items,
        returns_items: returning.is_some(),
        next_token: None,
        capacity: Capacity::write(consumed),
        change: Some(Change {
            event_name: "MODIFY",
            keys: key,
            old_image: Some(old),
            new_image: Some(new),
        }),
    })
}

fn delete(
    tables: &mut BTreeMap<String, DynamoTable>,
    source: &Source,
    filter: Option<&Expr>,
    returning: Option<Returning>,
    opts: &ExecOptions<'_>,
) -> Result<Outcome, ExecError> {
    let table = write_target(tables, source)?;
    let (key, condition) = pinned_key(table, filter)?;
    let mut outcome = Outcome {
        table_name: table.name.clone(),
        items: Vec::new(),
        returns_items: returning.is_some(),
        next_token: None,
        capacity: Capacity::write(Consumed::table(1.0)),
        change: None,
    };
    // A missing item is a no-op: there is nothing for the condition to fail on.
    let Some((id, old)) = find(table, &key) else {
        return Ok(outcome);
    };
    if !condition.iter().all(|c| test(c, old)) {
        return Err(ExecError::ran(
            condition_failed(Some(old), opts.return_old_on_condition_failure),
            &table.name,
        ));
    }
    outcome.capacity = Capacity::write(item_write_consumed(table, Some(old), None));
    let removed = table.remove_item_at(id);
    if returning.is_some() {
        outcome.items.push(removed.clone());
    }
    outcome.change = Some(Change {
        event_name: "REMOVE",
        keys: key,
        old_image: Some(removed),
        new_image: None,
    });
    Ok(outcome)
}

/// Scale a statement's capacity for a transaction (every unit counts twice).
pub(crate) fn transactional(mut capacity: Capacity) -> Capacity {
    capacity.consumed = capacity.consumed.scaled(2.0);
    capacity
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equality_treats_sets_as_unordered() {
        assert!(av_equal(
            &json!({"SS": ["a", "b"]}),
            &json!({"SS": ["b", "a"]})
        ));
        assert!(av_equal(
            &json!({"NS": ["1", "2.0"]}),
            &json!({"NS": ["2", "1"]})
        ));
        assert!(!av_equal(
            &json!({"L": [{"S": "a"}, {"S": "b"}]}),
            &json!({"L": [{"S": "b"}, {"S": "a"}]})
        ));
        assert!(av_equal(
            &json!({"M": {"x": {"N": "1"}, "y": {"N": "2"}}}),
            &json!({"M": {"y": {"N": "2.0"}, "x": {"N": "1"}}})
        ));
        assert!(!av_equal(&json!({"S": "1"}), &json!({"N": "1"})));
    }

    #[test]
    fn column_names_are_the_last_path_step() {
        let item: Item = serde_json::from_value(json!({
            "pk": {"S": "a"},
            "m": {"M": {"nested": {"S": "deep"}}}
        }))
        .unwrap();
        let paths = vec![
            vec![
                PathSegment::Name("m".into()),
                PathSegment::Name("nested".into()),
            ],
            vec![PathSegment::Name("pk".into())],
        ];
        assert_eq!(
            json!(project_columns(&item, &paths)),
            json!({"nested": {"S": "deep"}, "pk": {"S": "a"}})
        );
    }
}
