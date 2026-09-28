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

/// Resolve a (possibly dotted, possibly `#name`-containing) document path to
/// the leaf `AttributeValue` inside `item`. Single-segment paths (`foo`,
/// `#foo`) resolve to a top-level attribute. Dotted paths (`profile.email`,
/// `#p.#e`, `items[0].sku`) walk into `M`/`L` containers. Returns `None` if
/// any segment is missing or the intermediate value isn't a map/list.
pub(crate) fn resolve_path(
    path: &str,
    item: &HashMap<String, AttributeValue>,
    expr_attr_names: &HashMap<String, String>,
) -> Option<Value> {
    // Fast path: a single-segment expression (no `.` and no `[` in the raw
    // input) refers to a top-level attribute by its literal name, even if the
    // resolved alias contains a `.`. Without this, `#sw` -> `Safety.Warning`
    // would be misread as the nested path `Safety` -> `Warning`.
    if !path.contains('.') && !path.contains('[') {
        return item.get(&resolve_attr_name(path, expr_attr_names)).cloned();
    }
    let segs = resolve_projection_path_segments(path, expr_attr_names);
    resolve_nested_path_segments(item, &segs)
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
                        project_single_path_into(&mut result, item, raw, &names);
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

/// Project an item using a comma-separated `ProjectionExpression`,
/// resolving `#alias` references via `expr_attr_names`.
pub(crate) fn project_with_expression(
    item: &HashMap<String, AttributeValue>,
    proj: &str,
    expr_attr_names: &HashMap<String, String>,
) -> HashMap<String, AttributeValue> {
    let mut result = HashMap::new();
    for raw in proj.split(',') {
        project_single_path_into(&mut result, item, raw.trim(), expr_attr_names);
    }
    result
}

/// Resolve a single projection path against `item` and merge the result
/// into `result`. Shared by ProjectionExpression and AttributesToGet.
fn project_single_path_into(
    result: &mut HashMap<String, AttributeValue>,
    item: &HashMap<String, AttributeValue>,
    raw: &str,
    expr_attr_names: &HashMap<String, String>,
) {
    // Single-segment: treat as literal top-level attribute even if the
    // alias resolves to a name containing `.` (e.g. `#sw` ->
    // `Safety.Warning`).
    if !raw.contains('.') && !raw.contains('[') {
        let key = resolve_attr_name(raw, expr_attr_names);
        if let Some(v) = item.get(&key) {
            result.insert(key, v.clone());
        }
    } else {
        let segs = resolve_projection_path_segments(raw, expr_attr_names);
        if let Some(v) = resolve_nested_path_segments(item, &segs) {
            insert_nested_value_segments(result, &segs, v);
        }
    }
}

/// Resolve a projection path to logical `PathSegment`s, substituting
/// `#alias` references without re-splitting the resolved name. Use
/// this when the result will feed back into
/// [`resolve_nested_path_segments`] / [`insert_nested_value_segments`]
/// — otherwise an alias whose value contains `.` (e.g. `#sw` ->
/// `Safety.Warning`) produces extra spurious segments.
pub(crate) fn resolve_projection_path_segments(
    path: &str,
    expr_attr_names: &HashMap<String, String>,
) -> Vec<PathSegment> {
    let raw = parse_path_segments(path);
    raw.into_iter()
        .map(|seg| match seg {
            PathSegment::Key(k) => PathSegment::Key(resolve_attr_name(&k, expr_attr_names)),
            other => other,
        })
        .collect()
}

/// Like [`resolve_nested_path`] but operating on pre-resolved segments.
pub(crate) fn resolve_nested_path_segments(
    item: &HashMap<String, AttributeValue>,
    segments: &[PathSegment],
) -> Option<Value> {
    if segments.is_empty() {
        return None;
    }
    let top_key = match &segments[0] {
        PathSegment::Key(k) => k.as_str(),
        _ => return None,
    };
    let mut current = item.get(top_key)?.clone();
    for segment in &segments[1..] {
        match segment {
            PathSegment::Key(k) => {
                current = current.get("M")?.get(k)?.clone();
            }
            PathSegment::Index(idx) => {
                current = current.get("L")?.get(*idx)?.clone();
            }
        }
    }
    Some(current)
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

/// Resolve a potentially nested path like "a.b.c" or "a[0].b" from an item.
///
/// Kept for tests that exercise raw path parsing; production callers
/// should resolve aliases first via
/// [`resolve_projection_path_segments`] and then call
/// [`resolve_nested_path_segments`] directly.
#[cfg(test)]
pub(crate) fn resolve_nested_path(
    item: &HashMap<String, AttributeValue>,
    path: &str,
) -> Option<Value> {
    resolve_nested_path_segments(item, &parse_path_segments(path))
}

/// Parse a path like "a.b[0].c" into segments: [Key("a"), Key("b"), Index(0), Key("c")]
pub(crate) fn parse_path_segments(path: &str) -> Vec<PathSegment> {
    let mut segments = Vec::new();
    let mut current = String::new();

    let chars: Vec<char> = path.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '.' => {
                if !current.is_empty() {
                    segments.push(PathSegment::Key(current.clone()));
                    current.clear();
                }
            }
            '[' => {
                if !current.is_empty() {
                    segments.push(PathSegment::Key(current.clone()));
                    current.clear();
                }
                i += 1;
                let mut num = String::new();
                while i < chars.len() && chars[i] != ']' {
                    num.push(chars[i]);
                    i += 1;
                }
                if let Ok(idx) = num.parse::<usize>() {
                    segments.push(PathSegment::Index(idx));
                }
                // skip ']'
            }
            c => {
                current.push(c);
            }
        }
        i += 1;
    }
    if !current.is_empty() {
        segments.push(PathSegment::Key(current));
    }
    segments
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
