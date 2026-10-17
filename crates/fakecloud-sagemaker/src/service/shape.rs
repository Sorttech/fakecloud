//! Output-shape projection. Records are stored as the request that created
//! (or last updated) them, and several SageMaker read shapes differ from the
//! write shapes that share their member names: `DescribeHumanTaskUi.UiTemplate`
//! is a `UiTemplateInfo`, not the `UiTemplate` the create took;
//! `ListWorkforces` must never return the `OidcConfig.ClientSecret` the
//! create took; `WorkforceVpcConfigResponse` requires members its request
//! twin leaves optional; and so on. Every successful response is therefore
//! projected onto its operation's full output shape tree
//! ([`crate::generated::OSHAPES`]) before it reaches the wire:
//!
//! * [`project`] keeps only members the output shape declares, recursively
//!   (structures, list elements, map values), drops values whose JSON type
//!   does not match the modelled kind, and coerces timestamps to their
//!   epoch-second wire form;
//! * [`complete`] then fills every `@required` member still missing from a
//!   present structure (at any depth) with a shape-valid value, the same way
//!   the engine completes the top-level required members.
//!
//! Values AWS *derives* from the request (DescribeCluster's instance counts,
//! a UI template's content hash, ...) are produced before projection by
//! [`super::derive`].

use serde_json::{Map, Value};

use crate::generated::{OShape, OpMeta, K, OSHAPES, OUTPUT_SHAPES};

use super::engine::{coerce_ts, kind_matches, pick_enum, synth_str};
use super::{now_epoch, Ctx};

/// Nesting depth past which [`synth`] stops building required sub-structures
/// (a guard against required-member cycles; the model nests far shallower).
const MAX_SYNTH_DEPTH: usize = 8;

fn node(i: u16) -> &'static OShape {
    &OSHAPES[usize::from(i)]
}

/// The output shape of `op`, if it has one.
pub(crate) fn output_shape(op: &str) -> Option<&'static OShape> {
    OUTPUT_SHAPES
        .binary_search_by(|(name, _)| (*name).cmp(op))
        .ok()
        .map(|i| node(OUTPUT_SHAPES[i].1))
}

/// Project `v` onto `shape`: `None` when `v` cannot be represented as the
/// shape (wrong JSON type, null, or a union none of whose members survive).
pub(crate) fn project(shape: &OShape, v: &Value) -> Option<Value> {
    if v.is_null() {
        return None;
    }
    match shape.kind {
        K::Struct => {
            let obj = v.as_object()?;
            let mut out = Map::new();
            for m in shape.members {
                if let Some(p) = obj.get(m.wire).and_then(|c| project(node(m.shape), c)) {
                    out.insert(m.wire.to_string(), p);
                }
            }
            if shape.union && out.is_empty() {
                return None;
            }
            Some(Value::Object(out))
        }
        K::List => {
            let elem = node(shape.elem);
            Some(Value::Array(
                v.as_array()?
                    .iter()
                    .filter_map(|e| project(elem, e))
                    .collect(),
            ))
        }
        K::Map => {
            let value_shape = node(shape.elem);
            Some(Value::Object(
                v.as_object()?
                    .iter()
                    .filter_map(|(k, e)| project(value_shape, e).map(|p| (k.clone(), p)))
                    .collect(),
            ))
        }
        K::Ts => coerce_ts(v),
        kind => kind_matches(kind, v).then(|| v.clone()),
    }
}

/// Fill every `@required` member missing from a structure in `v` (at any
/// depth) with a synthesised, shape-valid value. `key` is the resource
/// identifier synthesised ARNs / names / ids derive from.
pub(crate) fn complete(ctx: &Ctx, meta: &OpMeta, key: &str, shape: &OShape, v: &mut Value) {
    match shape.kind {
        K::Struct => {
            let Some(obj) = v.as_object_mut() else { return };
            if !shape.union {
                for m in shape.members.iter().filter(|m| m.req) {
                    if obj.get(m.wire).is_none_or(Value::is_null) {
                        obj.insert(
                            m.wire.to_string(),
                            synth(ctx, meta, key, m.wire, node(m.shape), 0),
                        );
                    }
                }
            }
            for m in shape.members {
                if let Some(child) = obj.get_mut(m.wire) {
                    complete(ctx, meta, key, node(m.shape), child);
                }
            }
        }
        K::List => {
            if let Some(arr) = v.as_array_mut() {
                for e in arr {
                    // Each element is its own resource / entry: anything
                    // synthesised inside it derives from the element's own
                    // identifier, never from the request's.
                    let own = element_key(e);
                    complete(ctx, meta, &own, node(shape.elem), e);
                }
            }
        }
        K::Map => {
            if let Some(obj) = v.as_object_mut() {
                for e in obj.values_mut() {
                    complete(ctx, meta, key, node(shape.elem), e);
                }
            }
        }
        _ => {}
    }
}

/// A shape-valid value for a required member `wire` of shape `shape`.
fn synth(ctx: &Ctx, meta: &OpMeta, key: &str, wire: &str, shape: &OShape, depth: usize) -> Value {
    match shape.kind {
        K::Str | K::Blob => {
            Value::String(synth_str(ctx, meta, key, wire, shape.enums, shape.min_len))
        }
        K::Ts => now_epoch(),
        K::Int => match shape.int_enums.first() {
            Some(v) => Value::from(*v),
            None => Value::from(shape.min_val.unwrap_or(1)),
        },
        K::Num => Value::from(shape.min_val.unwrap_or(1) as f64),
        K::Bool => Value::Bool(true),
        K::Map => Value::Object(Map::new()),
        K::Struct => {
            let mut m = Map::new();
            if depth < MAX_SYNTH_DEPTH {
                if shape.union {
                    // A union carries exactly one member.
                    if let Some(first) = shape.members.first() {
                        m.insert(
                            first.wire.to_string(),
                            synth(ctx, meta, key, first.wire, node(first.shape), depth + 1),
                        );
                    }
                } else {
                    for f in shape.members.iter().filter(|f| f.req) {
                        m.insert(
                            f.wire.to_string(),
                            synth(ctx, meta, key, f.wire, node(f.shape), depth + 1),
                        );
                    }
                }
            }
            Value::Object(m)
        }
        K::List => {
            // One element only when the model requires a non-empty list.
            if shape.min_len.unwrap_or(0) >= 1 && depth < MAX_SYNTH_DEPTH {
                let elem = node(shape.elem);
                let v = match elem.kind {
                    K::Str | K::Blob if elem.enums.is_empty() => {
                        Value::String("placeholder".to_string())
                    }
                    K::Str | K::Blob => Value::String(pick_enum(elem.enums)),
                    _ => synth(ctx, meta, key, wire, elem, depth + 1),
                };
                Value::Array(vec![v])
            } else {
                Value::Array(Vec::new())
            }
        }
    }
}

/// The resource name an identifier stands for: an ARN
/// (`arn:aws:sagemaker:...:cluster/c1`) yields its resource name (`c1`),
/// so nothing synthesised from it nests one ARN inside another.
pub(crate) fn resource_name(arn_path: &str, id: &str) -> String {
    if !id.starts_with("arn:") {
        return id.to_string();
    }
    let resource = id.splitn(6, ':').nth(5).unwrap_or_default();
    match resource
        .strip_prefix(arn_path)
        .and_then(|r| r.strip_prefix('/'))
    {
        Some(name) if !arn_path.is_empty() => name.to_string(),
        _ => resource.rsplit('/').next().unwrap_or_default().to_string(),
    }
}

/// The identifier of a list element: its first `*Name`, else `*Id`, else
/// `*Arn` (as a resource name) string member; empty when it carries none.
pub(crate) fn element_key(el: &Value) -> String {
    let Some(obj) = el.as_object() else {
        return String::new();
    };
    for suffix in ["Name", "Id", "Arn"] {
        if let Some(v) = obj
            .iter()
            .find(|(k, v)| k.ends_with(suffix) && v.is_string())
            .and_then(|(_, v)| v.as_str())
        {
            return resource_name("", v);
        }
    }
    String::new()
}

/// The identifier synthesised output members derive from: the operation's
/// identifier member, else the request's first `*Arn` / `*Name` / `*Id`
/// member -- never an arbitrary request string such as a sort key or page
/// token -- as a resource name.
pub(crate) fn response_key(meta: &OpMeta, body: &Map<String, Value>) -> String {
    let named = body.get(meta.key_member).and_then(Value::as_str);
    let id = named.map(str::to_string).unwrap_or_else(|| {
        ["Arn", "Name", "Id"]
            .iter()
            .find_map(|suffix| {
                body.iter()
                    .find(|(k, v)| k.ends_with(suffix) && v.is_string())
                    .and_then(|(_, v)| v.as_str())
                    .map(str::to_string)
            })
            .unwrap_or_default()
    });
    resource_name(meta.arn_path, &id)
}

/// Project a whole response body onto `op`'s output shape and complete its
/// required members. Operations without an output shape are left alone.
pub(crate) fn shape_output(ctx: &Ctx, meta: &OpMeta, key: &str, out: Value) -> Value {
    let Some(shape) = output_shape(meta.op) else {
        return out;
    };
    let mut projected = project(shape, &out).unwrap_or_else(|| Value::Object(Map::new()));
    complete(ctx, meta, key, shape, &mut projected);
    projected
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generated::OPS;
    use serde_json::json;

    fn meta(op: &str) -> &'static OpMeta {
        OPS.iter().find(|m| m.op == op).unwrap()
    }

    fn ctx() -> Ctx {
        Ctx {
            account: "000000000000".to_string(),
            region: "us-east-1".to_string(),
        }
    }

    #[test]
    fn list_elements_synthesise_from_their_own_ids() {
        let m = meta("ListClusters");
        let out = shape_output(
            &ctx(),
            m,
            "request-key",
            json!({"ClusterSummaries": [{"ClusterName": "c1"}, {"ClusterName": "c2"}]}),
        );
        let s = &out["ClusterSummaries"];
        assert_eq!(
            s[0]["ClusterArn"],
            "arn:aws:sagemaker:us-east-1:000000000000:cluster/c1"
        );
        assert_eq!(
            s[1]["ClusterArn"],
            "arn:aws:sagemaker:us-east-1:000000000000:cluster/c2"
        );
    }

    #[test]
    fn the_response_key_is_an_identifier_never_an_arbitrary_string() {
        let body = json!({"SortBy": "Name", "SortOrder": "Ascending", "NextToken": "tok"});
        assert_eq!(
            response_key(meta("ListClusters"), body.as_object().unwrap()),
            ""
        );
        let body = json!({"ClusterName": "arn:aws:sagemaker:us-east-1:000000000000:cluster/c1"});
        let m = meta("UpdateCluster");
        let key = response_key(m, body.as_object().unwrap());
        assert_eq!(key, "c1");
        // An ARN-addressed update never nests an ARN inside an ARN.
        let out = shape_output(&ctx(), m, &key, json!({}));
        assert_eq!(
            out["ClusterArn"],
            "arn:aws:sagemaker:us-east-1:000000000000:cluster/c1"
        );
        let out = shape_output(
            &ctx(),
            m,
            "arn:aws:sagemaker:us-east-1:000000000000:cluster/c1",
            json!({}),
        );
        assert_eq!(
            out["ClusterArn"],
            "arn:aws:sagemaker:us-east-1:000000000000:cluster/c1"
        );
    }

    #[test]
    fn required_int_enum_leaves_synthesise_a_modelled_value() {
        let leaf = OSHAPES
            .iter()
            .find(|s| !s.int_enums.is_empty())
            .expect("the model has an intEnum");
        let v = synth(&ctx(), meta("DescribeEndpoint"), "", "Frequency", leaf, 0);
        assert!(leaf.int_enums.contains(&v.as_i64().unwrap()), "{v}");
    }
}
