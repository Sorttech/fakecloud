//! dynamodb helpers `metrics` concerns (audit-2026-05-19).

use super::*;

/// Capacity units consumed by one request against one table, broken down by
/// where they were spent: the base table and each secondary index.
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct Consumed {
    pub(crate) table: f64,
    pub(crate) gsi: BTreeMap<String, f64>,
    pub(crate) lsi: BTreeMap<String, f64>,
}

impl Consumed {
    /// Units spent on the base table alone.
    pub(crate) fn table(units: f64) -> Self {
        Consumed {
            table: units,
            ..Default::default()
        }
    }

    /// The aggregate across the table and every index.
    pub(crate) fn total(&self) -> f64 {
        self.table + self.gsi.values().sum::<f64>() + self.lsi.values().sum::<f64>()
    }

    /// Fold `other` into this breakdown.
    pub(crate) fn add(&mut self, other: &Consumed) {
        self.table += other.table;
        for (name, units) in &other.gsi {
            *self.gsi.entry(name.clone()).or_default() += units;
        }
        for (name, units) in &other.lsi {
            *self.lsi.entry(name.clone()).or_default() += units;
        }
    }

    /// Every figure multiplied by `factor` (a transaction doubles its units).
    pub(crate) fn scaled(mut self, factor: f64) -> Self {
        self.table *= factor;
        self.gsi.values_mut().for_each(|u| *u *= factor);
        self.lsi.values_mut().for_each(|u| *u *= factor);
        self
    }
}

/// Whether a `ConsumedCapacity` reports the read/write split next to each
/// aggregate. Only transactional operations do; every single-item and
/// single-table operation reports the aggregate alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CapacitySplit {
    None,
    Read,
    Write,
}

/// Read units for `bytes` read: one per 4KB started, halved for an
/// eventually-consistent read. A read always costs at least one unit.
pub(crate) fn read_units(bytes: usize, consistent: bool) -> f64 {
    let units = bytes.div_ceil(4096).max(1) as f64;
    if consistent {
        units
    } else {
        units * 0.5
    }
}

/// Write units for `bytes` written: one per 1KB started, at least one.
pub(crate) fn write_units(bytes: usize) -> f64 {
    bytes.div_ceil(1024).max(1) as f64
}

/// The write units a change from `old` to `new` costs the table's secondary
/// indexes, where `None` is an absent item.
///
/// An index is only charged when the change moves what it stores: an item
/// entering the index costs one write of its entry, leaving it costs one
/// write of the old entry, and a change of the index key costs both. An item
/// that stays with the same index key costs one write when its projected view
/// changed and nothing otherwise, so an identical overwrite charges no index.
pub(crate) fn index_write_units(
    table: &DynamoTable,
    old: Option<&HashMap<String, AttributeValue>>,
    new: Option<&HashMap<String, AttributeValue>>,
) -> Consumed {
    let table_hash = table.hash_key_name().to_string();
    let table_range = table.range_key_name().map(str::to_string);
    let charge = |key_schema: &[KeySchemaElement], projection: &Projection| -> f64 {
        let key_attrs: Vec<String> = key_schema
            .iter()
            .map(|k| k.attribute_name.clone())
            .collect();
        let entry = |item: Option<&HashMap<String, AttributeValue>>| {
            item.filter(|i| key_attrs.iter().all(|k| i.contains_key(k)))
                .map(|i| {
                    crate::service::queries::apply_index_projection(
                        i.clone(),
                        projection,
                        &key_attrs,
                        &table_hash,
                        table_range.as_deref(),
                    )
                })
        };
        match (entry(old), entry(new)) {
            (None, None) => 0.0,
            (Some(before), None) => write_units(item_size(&before)),
            (None, Some(after)) => write_units(item_size(&after)),
            (Some(before), Some(after)) => {
                let key_moved = key_attrs.iter().any(|k| before.get(k) != after.get(k));
                if key_moved {
                    write_units(item_size(&before)) + write_units(item_size(&after))
                } else if before != after {
                    write_units(item_size(&after))
                } else {
                    0.0
                }
            }
        }
    };
    let mut consumed = Consumed::default();
    for gsi in &table.gsi {
        let units = charge(&gsi.key_schema, &gsi.projection);
        if units > 0.0 {
            consumed.gsi.insert(gsi.index_name.clone(), units);
        }
    }
    for lsi in &table.lsi {
        let units = charge(&lsi.key_schema, &lsi.projection);
        if units > 0.0 {
            consumed.lsi.insert(lsi.index_name.clone(), units);
        }
    }
    consumed
}

/// What a single-item write from `old` to `new` consumes: the table write,
/// sized on the larger of the two images, plus every index it touched.
pub(crate) fn item_write_consumed(
    table: &DynamoTable,
    old: Option<&HashMap<String, AttributeValue>>,
    new: Option<&HashMap<String, AttributeValue>>,
) -> Consumed {
    let bytes = old
        .map(item_size)
        .unwrap_or(0)
        .max(new.map(item_size).unwrap_or(0));
    let mut consumed = index_write_units(table, old, new);
    consumed.table = write_units(bytes);
    consumed
}

/// Build a `ConsumedCapacity` JSON object in AWS's shape.
///
/// Mode is one of `"TOTAL" | "INDEXES"`; anything else returns `Value::Null`.
/// `TOTAL` reports the aggregate alone. `INDEXES` adds the `Table` arm and a
/// `GlobalSecondaryIndexes` / `LocalSecondaryIndexes` map only when an index
/// of that kind was charged -- an index the request cost nothing is absent,
/// never a zero entry. `split` adds the transactional read/write figure next
/// to every aggregate.
pub(crate) fn build_capacity(
    mode: &str,
    table_name: &str,
    consumed: &Consumed,
    split: CapacitySplit,
) -> Value {
    if mode != "TOTAL" && mode != "INDEXES" {
        return Value::Null;
    }
    let arm = |units: f64| {
        let mut arm = json!({ "CapacityUnits": units });
        match split {
            CapacitySplit::None => {}
            CapacitySplit::Read => arm["ReadCapacityUnits"] = json!(units),
            CapacitySplit::Write => arm["WriteCapacityUnits"] = json!(units),
        }
        arm
    };
    let mut cc = arm(consumed.total());
    cc["TableName"] = json!(table_name);
    if mode == "INDEXES" {
        cc["Table"] = arm(consumed.table);
        if !consumed.gsi.is_empty() {
            let map: serde_json::Map<String, Value> = consumed
                .gsi
                .iter()
                .map(|(name, units)| (name.clone(), arm(*units)))
                .collect();
            cc["GlobalSecondaryIndexes"] = Value::Object(map);
        }
        if !consumed.lsi.is_empty() {
            let map: serde_json::Map<String, Value> = consumed
                .lsi
                .iter()
                .map(|(name, units)| (name.clone(), arm(*units)))
                .collect();
            cc["LocalSecondaryIndexes"] = Value::Object(map);
        }
    }
    cc
}

/// Build the `ConsumedCapacity` for a request that consumed `read_units +
/// write_units` on the base table alone, reported as the aggregate without a
/// read/write split.
pub(crate) fn build_consumed_capacity(
    mode: &str,
    table_name: &str,
    read_units: f64,
    write_units: f64,
) -> Value {
    build_capacity(
        mode,
        table_name,
        &Consumed::table(read_units + write_units),
        CapacitySplit::None,
    )
}

/// Read the request body's `ReturnConsumedCapacity` value with default
/// `"NONE"`, returning the canonical mode string.
pub(crate) fn return_consumed_mode(body: &Value) -> &str {
    body["ReturnConsumedCapacity"].as_str().unwrap_or("NONE")
}

/// Read the request body's `ReturnItemCollectionMetrics` value with
/// default `"NONE"`.
pub(crate) fn return_icm_mode(body: &Value) -> &str {
    body["ReturnItemCollectionMetrics"]
        .as_str()
        .unwrap_or("NONE")
}

/// Build the per-write `ItemCollectionMetrics` document AWS emits when the
/// table has at least one local secondary index and the caller asked for
/// `ReturnItemCollectionMetrics=SIZE`. Returns `Value::Null` whenever the
/// document should be omitted.
///
/// `ItemCollectionKey` is the partition-key attribute of the affected item
/// — we look up the key-schema's HASH element and copy that attribute from
/// `key`. `SizeEstimateRangeGB` reports a coarse [lower, upper] estimate;
/// real DynamoDB returns 0..1 GB for small collections, so we use the same
/// range as a stand-in until item-collection sizing is tracked precisely.
pub(crate) fn build_item_collection_metrics(
    mode: &str,
    table: &crate::state::DynamoTable,
    key: &std::collections::HashMap<String, crate::state::AttributeValue>,
) -> Value {
    if mode != "SIZE" || table.lsi.is_empty() {
        return Value::Null;
    }
    let partition_key = table
        .key_schema
        .iter()
        .find(|k| k.key_type == "HASH")
        .map(|k| k.attribute_name.as_str());
    let mut item_collection_key = serde_json::Map::new();
    if let Some(pk_name) = partition_key {
        if let Some(pk_value) = key.get(pk_name) {
            if let Ok(serialized) = serde_json::to_value(pk_value) {
                item_collection_key.insert(pk_name.to_string(), serialized);
            }
        }
    }
    json!({
        "ItemCollectionKey": Value::Object(item_collection_key),
        "SizeEstimateRangeGB": [0.0, 1.0],
    })
}
