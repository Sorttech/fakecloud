//! dynamodb helpers `paths` concerns (audit-2026-05-19).

use super::*;

pub(crate) fn resolve_attr_name(name: &str, expr_attr_names: &HashMap<String, String>) -> String {
    let name = name.trim_matches('"');
    if name.starts_with('#') {
        expr_attr_names
            .get(name)
            .cloned()
            .unwrap_or_else(|| name.to_string())
    } else {
        name.to_string()
    }
}

/// Resolve a document path (`foo`, `#foo`, `profile.email`, `#p.#e`,
/// `items[0].sku`, `l[0][1]`) to the leaf `AttributeValue` inside `item`,
/// segmenting it with the expression grammar's own path parser. An alias is
/// one whole name, so `#sw` -> `Safety.Warning` is a top-level attribute.
/// Text the grammar does not accept (such as a quoted PartiQL name) is taken
/// as a literal top-level attribute name. Returns `None` if any step is
/// missing or the intermediate value isn't a map/list.
pub(crate) fn resolve_path(
    path: &str,
    item: &HashMap<String, AttributeValue>,
    expr_attr_names: &HashMap<String, String>,
) -> Option<Value> {
    match parse_document_path(path.trim(), expr_attr_names) {
        Some(doc) => resolve_doc_path(item, &doc).cloned(),
        None => item.get(&resolve_attr_name(path, expr_attr_names)).cloned(),
    }
}

pub(crate) fn project_item(
    item: &HashMap<String, AttributeValue>,
    body: &Value,
) -> HashMap<String, AttributeValue> {
    let projection = body["ProjectionExpression"].as_str();
    let mut result = match projection {
        Some(proj) if !proj.is_empty() => {
            let expr_attr_names = parse_expression_attribute_names(body);
            project_with_expression(item, proj, &expr_attr_names)
        }
        _ => {
            // Honor the deprecated `AttributesToGet` parameter the same
            // way real DynamoDB still does: it is a flat list of
            // (possibly document-path) attribute names with no `#alias`
            // substitution. Falls back to the whole item when neither
            // projection form is present.
            match body["AttributesToGet"].as_array() {
                Some(attrs) if !attrs.is_empty() => {
                    let names = HashMap::new();
                    let mut result = HashMap::new();
                    for raw in attrs.iter().filter_map(|v| v.as_str()) {
                        let path = parse_document_path(raw, &names)
                            .unwrap_or_else(|| vec![PathElem::Attr(raw.to_string())]);
                        project_doc_path_into(&mut result, item, &path);
                    }
                    result
                }
                _ => return item.clone(),
            }
        }
    };
    // A list-index projection (`MyList[2]`) builds a sparse `L` padded with
    // bare `Value::Null` placeholders; AWS returns only the projected elements,
    // compacted in index order. Drop the padding so the result is a valid,
    // compacted AttributeValue rather than one carrying bare nulls (bug-hunt
    // 2026-07-01, DynamoDB ProjectionExpression list-index).
    for v in result.values_mut() {
        compact_projected_lists(v);
    }
    result
}

/// Recursively remove the bare-`null` padding that `wrap_value_in_path` inserts
/// for list-index projections, so projected `L` values contain only the
/// requested elements (in index order). Real list elements are never a bare
/// JSON `null` (DynamoDB null is `{"NULL": true}`), so this only strips padding.
pub(crate) fn compact_projected_lists(value: &mut Value) {
    if let Some(list) = value.get_mut("L").and_then(Value::as_array_mut) {
        list.retain(|e| !e.is_null());
        for e in list.iter_mut() {
            compact_projected_lists(e);
        }
    } else if let Some(map) = value.get_mut("M").and_then(Value::as_object_mut) {
        for e in map.values_mut() {
            compact_projected_lists(e);
        }
    }
}

/// Project an item using a `ProjectionExpression`, parsed with the same
/// parser that validates it, resolving `#alias` references via
/// `expr_attr_names`.
pub(crate) fn project_with_expression(
    item: &HashMap<String, AttributeValue>,
    proj: &str,
    expr_attr_names: &HashMap<String, String>,
) -> HashMap<String, AttributeValue> {
    let values = HashMap::new();
    let mut ctx = ExprContext::new(expr_attr_names, &values, false);
    let paths = parse_projection_expression(proj, &mut ctx).unwrap_or_default();
    let mut result = HashMap::new();
    for path in &paths {
        project_doc_path_into(&mut result, item, path);
    }
    result
}

/// Copy the value at `path` (if present) from `item` into `result`, nested
/// under the same path. Shared by ProjectionExpression and AttributesToGet.
fn project_doc_path_into(
    result: &mut HashMap<String, AttributeValue>,
    item: &HashMap<String, AttributeValue>,
    path: &[PathElem],
) {
    let Some(v) = resolve_doc_path(item, path) else {
        return;
    };
    let segments: Vec<PathSegment> = path
        .iter()
        .map(|e| match e {
            PathElem::Attr(a) => PathSegment::Key(a.clone()),
            PathElem::Index(i) => PathSegment::Index(*i),
        })
        .collect();
    insert_nested_value_segments(result, &segments, v.clone());
}

/// Insert a value into `result` at the given pre-resolved segment path.
pub(crate) fn insert_nested_value_segments(
    result: &mut HashMap<String, AttributeValue>,
    segments: &[PathSegment],
    value: Value,
) {
    if segments.is_empty() {
        return;
    }
    let top_key = match &segments[0] {
        PathSegment::Key(k) => k.clone(),
        _ => return,
    };
    if segments.len() == 1 {
        result.insert(top_key, value);
        return;
    }
    let wrapped = wrap_value_in_path(&segments[1..], value);
    let existing = result.remove(&top_key);
    let merged = match existing {
        Some(existing) => merge_attribute_values(existing, wrapped),
        None => wrapped,
    };
    result.insert(top_key, merged);
}

/// Wrap a value in the nested path structure.
pub(crate) fn wrap_value_in_path(segments: &[PathSegment], value: Value) -> Value {
    if segments.is_empty() {
        return value;
    }
    let inner = wrap_value_in_path(&segments[1..], value);
    match &segments[0] {
        PathSegment::Key(k) => {
            json!({"M": {k.clone(): inner}})
        }
        PathSegment::Index(idx) => {
            let mut arr = vec![Value::Null; idx + 1];
            arr[*idx] = inner;
            json!({"L": arr})
        }
    }
}

/// Merge two attribute values (for overlapping projections).
///
/// Handles both `M` (map) and `L` (list) merging. Without the list
/// branch, two list-indexed projections (`list[0]` and `list[1]`)
/// would overwrite each other because the second projection's `L`
/// value replaced the first wholesale.
pub(crate) fn merge_attribute_values(a: Value, b: Value) -> Value {
    if let (Some(a_map), Some(b_map)) = (
        a.get("M").and_then(|v| v.as_object()),
        b.get("M").and_then(|v| v.as_object()),
    ) {
        let mut merged = a_map.clone();
        for (k, v) in b_map {
            if let Some(existing) = merged.get(k) {
                merged.insert(
                    k.clone(),
                    merge_attribute_values(existing.clone(), v.clone()),
                );
            } else {
                merged.insert(k.clone(), v.clone());
            }
        }
        return json!({"M": merged});
    }
    if let (Some(a_list), Some(b_list)) = (
        a.get("L").and_then(|v| v.as_array()),
        b.get("L").and_then(|v| v.as_array()),
    ) {
        let len = a_list.len().max(b_list.len());
        let mut out = Vec::with_capacity(len);
        for i in 0..len {
            let lhs = a_list.get(i).cloned().unwrap_or(Value::Null);
            let rhs = b_list.get(i).cloned().unwrap_or(Value::Null);
            // Null is the wrap_value_in_path placeholder for "no
            // projection touched this index" — prefer the non-null
            // side, recurse when both contributed a real value.
            let picked = if lhs.is_null() {
                rhs
            } else if rhs.is_null() {
                lhs
            } else {
                merge_attribute_values(lhs, rhs)
            };
            out.push(picked);
        }
        return json!({"L": out});
    }
    b
}

/// Validate a ProjectionExpression the way DynamoDB does before reading
/// anything, with the shared expression parser: it must parse as a
/// comma-separated list of document paths, every `#alias` must be defined, no
/// bare name may be a reserved word, and no two paths may overlap (one equal
/// to, or a prefix of, another once aliases are resolved).
pub(crate) fn validate_projection_expression(
    expr: &str,
    expr_attr_names: &HashMap<String, String>,
) -> Result<(), AwsServiceError> {
    let values = HashMap::new();
    let mut ctx = ExprContext::new(expr_attr_names, &values, true);
    parse_projection_expression(expr, &mut ctx).map(|_| ())
}

/// Validate the projection parameters of a read request block (a GetItem
/// body, a BatchGetItem KeysAndAttributes entry, a TransactGetItems Get):
/// the legacy `AttributesToGet` may not be combined with a
/// `ProjectionExpression`, and the expression itself must be valid.
pub(crate) fn validate_read_projection(block: &Value) -> Result<(), AwsServiceError> {
    let expression = block["ProjectionExpression"].as_str();
    if expression.is_some() && block.get("AttributesToGet").is_some_and(|v| !v.is_null()) {
        return Err(AwsServiceError::aws_error(
            StatusCode::BAD_REQUEST,
            "ValidationException",
            "Can not use both expression and non-expression parameters in the same request: \
             Non-expression parameters: {AttributesToGet} Expression parameters: \
             {ProjectionExpression}",
        ));
    }
    match expression {
        Some(expr) => {
            validate_projection_expression(expr, &parse_expression_attribute_names(block))
        }
        None => Ok(()),
    }
}

#[cfg(test)]
mod projection_validation_tests {
    use super::*;

    fn err(expr: &str, names: &[(&str, &str)]) -> String {
        let names: HashMap<String, String> = names
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        validate_projection_expression(expr, &names)
            .unwrap_err()
            .message()
            .to_string()
    }

    #[test]
    fn accepts_well_formed_paths() {
        let names = HashMap::from([("#a".to_string(), "a".to_string())]);
        for expr in ["a", "a, b", "#a.b, c[0].d", "l[0], l[1]", "a.b, a.c"] {
            assert!(
                validate_projection_expression(expr, &names).is_ok(),
                "{expr}"
            );
        }
    }

    #[test]
    fn reports_syntax_errors_with_token_and_context() {
        assert_eq!(
            err("!!!", &[]),
            "Invalid ProjectionExpression: Syntax error; token: \"!\", near: \"!!\""
        );
        assert_eq!(
            err("a b", &[]),
            "Invalid ProjectionExpression: Syntax error; token: \"b\", near: \"a b\""
        );
        assert_eq!(
            err("a,", &[]),
            "Invalid ProjectionExpression: Syntax error; token: \"<EOF>\", near: \",\""
        );
    }

    #[test]
    fn rejects_empty_expression() {
        for expr in ["", "   "] {
            assert_eq!(
                err(expr, &[]),
                "Invalid ProjectionExpression: The expression can not be empty;"
            );
        }
    }

    #[test]
    fn reports_undefined_alias() {
        assert_eq!(
            err("#undef", &[]),
            "Invalid ProjectionExpression: An expression attribute name used in the document \
             path is not defined; attribute name: #undef"
        );
    }

    #[test]
    fn reports_overlapping_paths_in_request_order() {
        let overlap = |one: &str, two: &str| {
            format!(
                "Invalid ProjectionExpression: Two document paths overlap with each other; must \
                 remove or rewrite one of these paths; path one: {one}, path two: {two}"
            )
        };
        assert_eq!(err("a, a", &[]), overlap("[a]", "[a]"));
        assert_eq!(err("a, a.b", &[]), overlap("[a]", "[a, b]"));
        assert_eq!(err("a.b, a", &[]), overlap("[a, b]", "[a]"));
        assert_eq!(
            err("#a, #b", &[("#a", "a"), ("#b", "a")]),
            overlap("[a]", "[a]")
        );
    }
}

#[cfg(test)]
mod projection_tests {
    use super::*;
    use serde_json::json;

    // Projection uses the same parser as validation, so whitespace inside a
    // path and a subscript on an alias behave as in any other expression.
    #[test]
    fn projection_paths_share_the_expression_parser() {
        let mut item: HashMap<String, AttributeValue> = HashMap::new();
        item.insert("l".into(), json!({"L": [{"S": "x"}, {"S": "y"}]}));
        item.insert(
            "a".into(),
            json!({"M": {"b": {"S": "ab"}, "c": {"S": "ac"}}}),
        );
        item.insert(
            "n".into(),
            json!({"L": [{"M": {"x": {"S": "n0"}}}, {"M": {"x": {"S": "n1"}, "y": {"S": "no"}}}]}),
        );
        let projected = project_item(
            &item,
            &json!({
                "ProjectionExpression": "l[ 0 ], a . b, #n[1].x",
                "ExpressionAttributeNames": {"#n": "n"},
            }),
        );
        assert_eq!(projected["l"], json!({"L": [{"S": "x"}]}));
        assert_eq!(projected["a"], json!({"M": {"b": {"S": "ab"}}}));
        assert_eq!(projected["n"], json!({"L": [{"M": {"x": {"S": "n1"}}}]}));
    }

    #[test]
    fn list_index_projection_compacts_padding() {
        // Projecting `items[2]` used to emit {"L":[null,null,elem]}; AWS returns
        // only the projected element, compacted (bug-hunt 2026-07-01).
        let mut item: HashMap<String, AttributeValue> = HashMap::new();
        item.insert(
            "items".to_string(),
            json!({"L": [{"S": "a"}, {"S": "b"}, {"S": "c"}]}),
        );
        let body = json!({"ProjectionExpression": "items[2]"});
        let projected = project_item(&item, &body);
        assert_eq!(projected["items"], json!({"L": [{"S": "c"}]}));
    }

    #[test]
    fn multiple_list_indices_project_in_order() {
        let mut item: HashMap<String, AttributeValue> = HashMap::new();
        item.insert(
            "items".to_string(),
            json!({"L": [{"S": "a"}, {"S": "b"}, {"S": "c"}]}),
        );
        let body = json!({"ProjectionExpression": "items[0], items[2]"});
        let projected = project_item(&item, &body);
        assert_eq!(projected["items"], json!({"L": [{"S": "a"}, {"S": "c"}]}));
    }
}
