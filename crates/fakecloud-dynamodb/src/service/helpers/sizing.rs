//! dynamodb helpers `sizing` concerns: how DynamoDB measures an item against
//! the 400KB limit and for consumed capacity, and the canonical form it stores
//! numbers in.

use super::*;

/// The item-size gate. An item of exactly this many bytes is accepted and one
/// byte more is refused.
pub(crate) const MAX_ITEM_SIZE: usize = 409_600;

/// UpdateItem's fixed charge for the update itself.
const UPDATE_BASE_COST: usize = 3;
/// UpdateItem's per-clause charge for a `SET`/`ADD` (or legacy `PUT`/`ADD`).
const UPDATE_WRITE_CLAUSE_COST: usize = 19;
/// UpdateItem's per-clause charge for a `REMOVE`/`DELETE`.
const UPDATE_REMOVE_CLAUSE_COST: usize = 2;

/// The error a put-shaped write (PutItem, BatchWriteItem, a transacted Put)
/// returns for an item over the limit.
pub(crate) fn put_item_too_large() -> AwsServiceError {
    AwsServiceError::aws_error(
        StatusCode::BAD_REQUEST,
        "ValidationException",
        "Item size has exceeded the maximum allowed size",
    )
}

/// The error an update-shaped write (UpdateItem, a transacted Update) returns
/// for an item over the limit.
pub(crate) fn update_item_too_large() -> AwsServiceError {
    AwsServiceError::aws_error(
        StatusCode::BAD_REQUEST,
        "ValidationException",
        "Item size to update has exceeded the maximum allowed size",
    )
}

/// Reject a put-shaped item over 400KB.
pub(crate) fn check_put_item_size(
    item: &HashMap<String, AttributeValue>,
) -> Result<(), AwsServiceError> {
    if item_size(item) > MAX_ITEM_SIZE {
        return Err(put_item_too_large());
    }
    Ok(())
}

/// Reject a finished item over 400KB on an update surface that measures the
/// item flat rather than charging per clause (a transacted Update).
pub(crate) fn check_update_item_size(
    item: &HashMap<String, AttributeValue>,
) -> Result<(), AwsServiceError> {
    if item_size(item) > MAX_ITEM_SIZE {
        return Err(update_item_too_large());
    }
    Ok(())
}

/// What a standalone UpdateItem is charged against the limit on top of the
/// finished item: the top-level attributes its clauses write and a fixed cost
/// per clause.
///
/// UpdateItem does not measure only the item it is about to store. It also
/// measures what the statement writes -- each touched attribute's name and
/// finished value -- plus 3 bytes for the update and a per-clause amount (19
/// for a `SET`/`ADD`, one more when the target goes through a list index, 2
/// for a `REMOVE`/`DELETE`). The key and any attribute the statement leaves
/// alone are outside that figure; the finished item is still capped at 400KB
/// as on every other surface.
#[derive(Debug, Default)]
pub(crate) struct UpdateCharge {
    touched: Vec<String>,
    clause_cost: usize,
}

impl UpdateCharge {
    /// The charge for an `UpdateExpression`.
    pub(crate) fn for_expression(expr: &str, expr_attr_names: &HashMap<String, String>) -> Self {
        let mut charge = UpdateCharge::default();
        for (action, assignments) in parse_update_clauses(expr) {
            for assignment in assignments {
                let path = match action {
                    UpdateAction::Set => assignment.split_once('=').map(|(l, _)| l),
                    UpdateAction::Remove => Some(assignment.as_str()),
                    UpdateAction::Add | UpdateAction::Delete => {
                        assignment.split_whitespace().next()
                    }
                };
                let Some(path) = path.map(str::trim).filter(|p| !p.is_empty()) else {
                    continue;
                };
                charge.clause_cost += match action {
                    UpdateAction::Set | UpdateAction::Add => {
                        UPDATE_WRITE_CLAUSE_COST + usize::from(path.contains('['))
                    }
                    UpdateAction::Remove | UpdateAction::Delete => UPDATE_REMOVE_CLAUSE_COST,
                };
                let top = path.split(['.', '[']).next().unwrap_or(path).trim();
                charge.touch(resolve_attr_name(top, expr_attr_names));
            }
        }
        charge
    }

    /// The charge for a legacy `AttributeUpdates` map.
    pub(crate) fn for_attribute_updates(updates: &serde_json::Map<String, Value>) -> Self {
        let mut charge = UpdateCharge::default();
        for (name, update) in updates {
            let action = update["Action"].as_str().unwrap_or("PUT");
            charge.clause_cost += if action == "DELETE" {
                UPDATE_REMOVE_CLAUSE_COST
            } else {
                UPDATE_WRITE_CLAUSE_COST
            };
            charge.touch(name.clone());
        }
        charge
    }

    fn touch(&mut self, name: String) {
        if !self.touched.contains(&name) {
            self.touched.push(name);
        }
    }

    /// Reject `item`, the finished item of this update, when it or the
    /// update's own charge exceeds the limit.
    pub(crate) fn check(
        &self,
        item: &HashMap<String, AttributeValue>,
    ) -> Result<(), AwsServiceError> {
        let written: usize = self
            .touched
            .iter()
            .filter_map(|name| item.get(name).map(|v| name.len() + attribute_value_size(v)))
            .sum();
        let charged = written + UPDATE_BASE_COST + self.clause_cost;
        if charged > MAX_ITEM_SIZE || item_size(item) > MAX_ITEM_SIZE {
            return Err(update_item_too_large());
        }
        Ok(())
    }
}

/// An item's size: every attribute's name plus its value.
pub(crate) fn item_size(item: &HashMap<String, AttributeValue>) -> usize {
    item.iter()
        .map(|(name, value)| name.len() + attribute_value_size(value))
        .sum()
}

/// One attribute value's contribution to an item's size.
///
/// Strings are their UTF-8 length and binaries their decoded length. A number
/// is sized by its significant digits (see [`number_size`]). A list or map
/// costs 3 bytes plus one byte per element on top of its elements (and, for a
/// map, their names). A boolean or null is one byte.
pub(crate) fn attribute_value_size(value: &Value) -> usize {
    let Some((tag, inner)) = value.as_object().and_then(|o| o.iter().next()) else {
        return 0;
    };
    let strings = || {
        inner
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
    };
    match tag.as_str() {
        "S" => inner.as_str().map_or(0, str::len),
        "N" => inner.as_str().map_or(0, number_size),
        "B" => inner.as_str().map_or(0, binary_size),
        "BOOL" | "NULL" => 1,
        "SS" => strings().map(str::len).sum(),
        "NS" => strings().map(number_size).sum(),
        "BS" => strings().map(binary_size).sum(),
        "L" => {
            3 + inner
                .as_array()
                .into_iter()
                .flatten()
                .map(|el| 1 + attribute_value_size(el))
                .sum::<usize>()
        }
        "M" => {
            3 + inner
                .as_object()
                .into_iter()
                .flatten()
                .map(|(name, el)| name.len() + 1 + attribute_value_size(el))
                .sum::<usize>()
        }
        _ => 0,
    }
}

/// The decoded length of a base64-encoded binary value.
fn binary_size(encoded: &str) -> usize {
    let len = encoded.len();
    if len.is_multiple_of(4)
        && encoded
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"+/=".contains(&b))
    {
        let padding = encoded.bytes().rev().take_while(|b| *b == b'=').count();
        return (len / 4 * 3).saturating_sub(padding);
    }
    base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_or(len, |bytes| bytes.len())
}

/// A number's size in bytes.
///
/// DynamoDB stores a number as base-100 digit pairs aligned on the decimal
/// point, with leading and trailing all-zero pairs dropped, plus one byte of
/// exponent and one more when it is negative. Zero is a single byte whatever
/// its sign. The exponent is not counted, so `1E125` costs what `1` does, and
/// digits straddling the decimal point fall into two pairs: `3.14159` costs 5
/// where the integer `123456` costs 4.
pub(crate) fn number_size(literal: &str) -> usize {
    let Some(canonical) = canonical_number(literal) else {
        return literal.len();
    };
    let (negative, magnitude) = match canonical.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, canonical.as_str()),
    };
    let (int_part, frac_part) = magnitude.split_once('.').unwrap_or((magnitude, ""));
    let int_part = int_part.trim_start_matches('0');
    if int_part.is_empty() && frac_part.is_empty() {
        return 1;
    }
    let mut digits = String::with_capacity(int_part.len() + frac_part.len() + 2);
    if int_part.len() % 2 == 1 {
        digits.push('0');
    }
    digits.push_str(int_part);
    digits.push_str(frac_part);
    if frac_part.len() % 2 == 1 {
        digits.push('0');
    }
    let pairs: Vec<&[u8]> = digits.as_bytes().chunks(2).collect();
    let first = pairs.iter().position(|p| *p != b"00");
    let last = pairs.iter().rposition(|p| *p != b"00");
    let significant = match (first, last) {
        (Some(f), Some(l)) => l - f + 1,
        _ => 0,
    };
    1 + significant + usize::from(negative)
}

/// Reject a well-formed number whose magnitude falls outside DynamoDB's
/// supported range: above `9.9999999999999999999999999999999999999E+125` or
/// below `1E-130`. Zero is always in range.
pub(crate) fn check_number_range(literal: &str) -> Result<(), AwsServiceError> {
    let Some(canonical) = canonical_number(literal) else {
        return Ok(());
    };
    let magnitude = canonical.trim_start_matches('-');
    let (int_part, frac_part) = magnitude.split_once('.').unwrap_or((magnitude, ""));
    let int_part = int_part.trim_start_matches('0');
    if !int_part.is_empty() {
        if int_part.len() - 1 > 125 {
            return Err(AwsServiceError::aws_error(
                StatusCode::BAD_REQUEST,
                "ValidationException",
                "Number overflow. Attempting to store a number with magnitude larger than supported range",
            ));
        }
    } else if !frac_part.is_empty() {
        let leading_zeros = frac_part.len() - frac_part.trim_start_matches('0').len();
        if leading_zeros + 1 > 130 {
            return Err(AwsServiceError::aws_error(
                StatusCode::BAD_REQUEST,
                "ValidationException",
                "Number underflow. Attempting to store a number with magnitude smaller than supported range",
            ));
        }
    }
    Ok(())
}

/// Rewrite every number in `item` (at any depth, sets included) into the
/// canonical form DynamoDB stores and returns: no sign on a positive value or
/// on zero, no leading or trailing zeros, and the exponent expanded, so
/// `+1.5E+3` reads back as `1500` and `0042.1200` as `42.12`.
pub(crate) fn normalize_item_numbers(item: &mut HashMap<String, AttributeValue>) {
    for value in item.values_mut() {
        normalize_value_numbers(value);
    }
}

/// [`normalize_item_numbers`] for a single attribute value.
pub(crate) fn normalize_value_numbers(value: &mut Value) {
    let Some(obj) = value.as_object_mut() else {
        return;
    };
    for (tag, inner) in obj.iter_mut() {
        match tag.as_str() {
            "N" => {
                if let Some(canonical) = inner.as_str().and_then(canonical_number) {
                    *inner = Value::String(canonical);
                }
            }
            "NS" => {
                for member in inner.as_array_mut().into_iter().flatten() {
                    if let Some(canonical) = member.as_str().and_then(canonical_number) {
                        *member = Value::String(canonical);
                    }
                }
            }
            "L" => {
                for el in inner.as_array_mut().into_iter().flatten() {
                    normalize_value_numbers(el);
                }
            }
            "M" => {
                for el in inner
                    .as_object_mut()
                    .into_iter()
                    .flat_map(|m| m.values_mut())
                {
                    normalize_value_numbers(el);
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The byte costs measured against the 400KB gate for each literal.
    #[test]
    fn number_size_matches_measured_costs() {
        let captured: &[(&str, usize)] = &[
            ("0", 1),
            ("1", 2),
            ("12", 2),
            ("123", 3),
            ("1234", 3),
            ("12345678901234567890123456789012345678", 20),
            ("0042", 2),
            ("100", 2),
            ("1010", 3),
            ("0.0000001", 2),
            ("1E125", 2),
            ("1E-100", 2),
            ("1.5", 3),
            ("15", 2),
            ("1.2", 3),
            ("1.200", 3),
            ("1.234", 4),
            ("3.14159", 5),
            ("123456", 4),
            ("100.5", 4),
            ("0.15", 2),
            ("-42", 3),
            ("-0", 1),
        ];
        for (literal, bytes) in captured {
            assert_eq!(number_size(literal), *bytes, "{literal}");
        }
    }

    #[test]
    fn item_size_counts_names_values_and_documents() {
        let item: HashMap<String, Value> = serde_json::from_value(json!({
            "pk": {"S": "abc"},
            "n": {"N": "123"},
            "b": {"B": "AAEC"},
            "flag": {"BOOL": true},
            "l": {"L": [{"S": "x"}, {"NULL": true}]},
            "m": {"M": {"k": {"S": "vv"}}},
            "ss": {"SS": ["a", "bc"]},
        }))
        .unwrap();
        // pk 2+3, n 1+3, b 1+3, flag 4+1, l 1+(3+2+2), m 1+(3+1+1+2), ss 2+3
        assert_eq!(item_size(&item), 5 + 4 + 4 + 5 + 8 + 8 + 5);
    }

    #[test]
    fn number_range_limits() {
        assert!(check_number_range("9.9999999999999999999999999999999999999E+125").is_ok());
        assert!(check_number_range("-9.9999999999999999999999999999999999999E+125").is_ok());
        assert!(check_number_range("1E-130").is_ok());
        assert!(check_number_range("0").is_ok());
        let over = check_number_range("1E+126").unwrap_err();
        assert!(over.message().starts_with("Number overflow"));
        assert!(check_number_range("-1E+126").is_err());
        let under = check_number_range("1E-131").unwrap_err();
        assert!(under.message().starts_with("Number underflow"));
    }

    #[test]
    fn numbers_normalize_to_canonical_form() {
        let mut item: HashMap<String, Value> = serde_json::from_value(json!({
            "a": {"N": "+1.5E+3"},
            "b": {"N": "-0"},
            "c": {"NS": ["0042.1200", ".5"]},
            "d": {"L": [{"N": "1e2"}, {"M": {"x": {"N": "5."}}}]},
        }))
        .unwrap();
        normalize_item_numbers(&mut item);
        assert_eq!(item["a"], json!({"N": "1500"}));
        assert_eq!(item["b"], json!({"N": "0"}));
        assert_eq!(item["c"], json!({"NS": ["42.12", "0.5"]}));
        assert_eq!(
            item["d"],
            json!({"L": [{"N": "100"}, {"M": {"x": {"N": "5"}}}]})
        );
    }

    #[test]
    fn update_charge_counts_clauses_and_written_attributes() {
        let names: HashMap<String, String> =
            [("#a".to_string(), "b".to_string())].into_iter().collect();
        let charge = UpdateCharge::for_expression("SET #a = :p, c[0] = :c REMOVE r", &names);
        assert_eq!(charge.clause_cost, 19 + 20 + 2);
        assert_eq!(charge.touched, vec!["b", "c", "r"]);

        // One-byte key: the charge, not the finished item, binds.
        let pad = MAX_ITEM_SIZE - 3 - 1 - 19;
        let charge = UpdateCharge::for_expression("SET b = :p", &HashMap::new());
        let item = |len: usize| -> HashMap<String, Value> {
            serde_json::from_value(json!({"pk": {"S": "K"}, "b": {"S": "x".repeat(len)}})).unwrap()
        };
        assert!(charge.check(&item(pad)).is_ok());
        let err = charge.check(&item(pad + 1)).unwrap_err();
        assert!(err.message().contains("Item size to update"));
    }
}
