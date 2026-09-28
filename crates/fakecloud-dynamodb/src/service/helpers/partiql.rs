//! dynamodb helpers `partiql` concerns (audit-2026-05-19): the number and
//! attribute-value comparisons PartiQL shares with the expression language.
//! The statement parser is `partiql_parse`, the executor `partiql_exec`.

use super::*;

/// Compare two DynamoDB numeric attribute strings with full precision.
///
/// DynamoDB numbers are arbitrary-precision decimals; parsing to `f64`
/// rounds past 2^53. This compares the decimal representations directly:
/// sign, then integer magnitude (by length, then lexically), then the
/// fractional part. Falls back to `Equal` only if both sides are
/// unparseable. Handles exponent notation and negative zero.
pub(crate) fn compare_number_strings(x: &str, y: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    // A malformed Number string has no defined ordering; callers that care about
    // equality (values_equal) pre-check validity, so leaving this Equal keeps
    // range/order comparisons undefined rather than inventing a lexical order.
    let (xn, xd) = match normalize_decimal(x) {
        Some(v) => v,
        None => return Ordering::Equal,
    };
    let (yn, yd) = match normalize_decimal(y) {
        Some(v) => v,
        None => return Ordering::Equal,
    };
    // (is_negative_x, is_negative_y): a negative value is always less than a
    // non-negative one. Only decide by sign when the signs actually differ.
    match (xn, yn) {
        (true, false) => return Ordering::Less,
        (false, true) => return Ordering::Greater,
        _ => {}
    }
    let mag = compare_magnitude(&xd, &yd);
    if xn {
        mag.reverse()
    } else {
        mag
    }
}

/// Whether `s` is a well-formed DynamoDB Number literal (a decimal string that
/// `normalize_decimal` can parse). Used to reject malformed `{"N":...}` operands
/// before they are persisted (ADD on a new attribute, bug-hunt 2026-07-01).
pub(crate) fn is_valid_number(s: &str) -> bool {
    normalize_decimal(s).is_some()
}

/// Number of significant digits in a DynamoDB Number literal, or `None` if the
/// string is not a valid number. Leading zeros, trailing zeros, the sign, the
/// decimal point and the exponent are all insignificant — DynamoDB stores a
/// number as a coefficient plus an exponent, so `1E38` (a 39-character string)
/// is a single significant digit while `1234...` with 39 non-zero digits is 39.
/// AWS rejects a Number with more than 38 significant digits.
pub(crate) fn significant_digit_count(s: &str) -> Option<usize> {
    let (_, (int_norm, frac_norm)) = normalize_decimal(s)?;
    // `int_norm` already has leading zeros stripped and `frac_norm` trailing
    // zeros stripped; the remaining leading zeros (value < 1) and trailing
    // zeros (integer with a zeroed tail) collapse into the exponent.
    let coeff = format!("{int_norm}{frac_norm}");
    Some(coeff.trim_start_matches('0').trim_end_matches('0').len())
}

/// Canonical decimal form of a DynamoDB Number, so numerically-equal literals
/// (`"1"`, `"1.0"`, `"1e0"`) collapse to one key. Used to detect duplicate
/// members in a Number Set (`NS`), which AWS compares by numeric value rather
/// than string. Returns `None` for a malformed number.
pub(crate) fn canonical_number(s: &str) -> Option<String> {
    let (neg, (int_part, frac_part)) = normalize_decimal(s)?;
    let sign = if neg { "-" } else { "" };
    let int_str = if int_part.is_empty() { "0" } else { &int_part };
    if frac_part.is_empty() {
        Some(format!("{sign}{int_str}"))
    } else {
        Some(format!("{sign}{int_str}.{frac_part}"))
    }
}

/// Decompose a decimal string into `(is_negative, (int_digits, frac_digits))`
/// with insignificant zeros stripped so the magnitude compare is purely
/// lexical. Returns `None` for non-numeric input; negative zero normalizes
/// to positive.
#[allow(clippy::type_complexity)]
fn normalize_decimal(s: &str) -> Option<(bool, (String, String))> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (neg, rest) = match s.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    let (mantissa, exp) = match rest.split_once(['e', 'E']) {
        Some((m, e)) => (m, e.parse::<i64>().ok()?),
        None => (rest, 0),
    };
    let (int_part, frac_part) = match mantissa.split_once('.') {
        Some((i, f)) => (i, f),
        None => (mantissa, ""),
    };
    if int_part.is_empty() && frac_part.is_empty() {
        return None;
    }
    if !int_part.chars().all(|c| c.is_ascii_digit())
        || !frac_part.chars().all(|c| c.is_ascii_digit())
    {
        return None;
    }
    let mut digits = format!("{int_part}{frac_part}");
    let point = int_part.len() as i64 + exp;
    let (int_str, frac_str) = if point <= 0 {
        let pad = "0".repeat((-point) as usize);
        digits = format!("{pad}{digits}");
        (String::new(), digits)
    } else if (point as usize) >= digits.len() {
        let pad = "0".repeat(point as usize - digits.len());
        digits.push_str(&pad);
        (digits, String::new())
    } else {
        let (i, f) = digits.split_at(point as usize);
        (i.to_string(), f.to_string())
    };
    let int_norm = int_str.trim_start_matches('0').to_string();
    let frac_norm = frac_str.trim_end_matches('0').to_string();
    let is_zero = int_norm.is_empty() && frac_norm.is_empty();
    Some((neg && !is_zero, (int_norm, frac_norm)))
}

/// Lexically compare two normalized non-negative magnitudes
/// `(int_digits, frac_digits)`.
fn compare_magnitude(a: &(String, String), b: &(String, String)) -> std::cmp::Ordering {
    a.0.len()
        .cmp(&b.0.len())
        .then_with(|| a.0.cmp(&b.0))
        .then_with(|| a.1.cmp(&b.1))
}

/// Add (`is_add`) or subtract two DynamoDB Number strings with arbitrary
/// precision, returning the normalized result string.
///
/// DynamoDB numbers are 38-significant-digit decimals; parsing operands to
/// `f64` rounds past 2^53 and saturates an `as i64` cast past ~9.2e18, so a
/// `SET #c = #c + :inc` or `ADD #c :inc` on a large counter silently corrupts
/// the value. This works directly on the decimal digit strings instead.
/// Returns `None` only if either operand is not a valid number.
/// bug-audit 2026-06-28.
pub(crate) fn decimal_add_sub(a: &str, b: &str, is_add: bool) -> Option<String> {
    let (a_neg, (a_int, a_frac)) = normalize_decimal(a)?;
    let (mut b_neg, (b_int, b_frac)) = normalize_decimal(b)?;
    if !is_add {
        b_neg = !b_neg;
    }

    let (res_neg, (res_int, res_frac)) = if a_neg == b_neg {
        // Same sign: add magnitudes, keep the sign.
        (a_neg, add_magnitude(&a_int, &a_frac, &b_int, &b_frac))
    } else {
        // Opposite signs: subtract the smaller magnitude from the larger and
        // take the larger's sign.
        match compare_magnitude(
            &(a_int.clone(), a_frac.clone()),
            &(b_int.clone(), b_frac.clone()),
        ) {
            std::cmp::Ordering::Equal => (false, (String::new(), String::new())),
            std::cmp::Ordering::Greater => (a_neg, sub_magnitude(&a_int, &a_frac, &b_int, &b_frac)),
            std::cmp::Ordering::Less => (b_neg, sub_magnitude(&b_int, &b_frac, &a_int, &a_frac)),
        }
    };

    Some(format_decimal(res_neg, &res_int, &res_frac))
}

/// Right-pad a fractional digit string with zeros to `len`.
fn pad_frac(frac: &str, len: usize) -> String {
    let mut s = frac.to_string();
    s.extend(std::iter::repeat_n('0', len.saturating_sub(s.len())));
    s
}

/// Split a digit string into `(int, frac)` where `frac` is the last
/// `frac_len` digits (front-padded with zeros if the string is too short).
fn split_at_frac(digits: &str, frac_len: usize) -> (String, String) {
    if digits.len() <= frac_len {
        let padded = format!("{}{}", "0".repeat(frac_len - digits.len()), digits);
        (String::new(), padded)
    } else {
        let (i, f) = digits.split_at(digits.len() - frac_len);
        (i.to_string(), f.to_string())
    }
}

/// Add two non-negative magnitudes given as `(int_digits, frac_digits)`.
fn add_magnitude(ai: &str, af: &str, bi: &str, bf: &str) -> (String, String) {
    let flen = af.len().max(bf.len());
    let a_digits = format!("{ai}{}", pad_frac(af, flen));
    let b_digits = format!("{bi}{}", pad_frac(bf, flen));
    split_at_frac(&add_digit_strings(&a_digits, &b_digits), flen)
}

/// Subtract magnitude `(bi, bf)` from `(ai, af)`, assuming the first is
/// greater than or equal to the second.
fn sub_magnitude(ai: &str, af: &str, bi: &str, bf: &str) -> (String, String) {
    let flen = af.len().max(bf.len());
    let a_digits = format!("{ai}{}", pad_frac(af, flen));
    let b_digits = format!("{bi}{}", pad_frac(bf, flen));
    split_at_frac(&sub_digit_strings(&a_digits, &b_digits), flen)
}

/// Schoolbook addition of two non-negative integer digit strings.
fn add_digit_strings(a: &str, b: &str) -> String {
    let a: Vec<u8> = a.bytes().rev().map(|c| c - b'0').collect();
    let b: Vec<u8> = b.bytes().rev().map(|c| c - b'0').collect();
    let n = a.len().max(b.len());
    let mut out = Vec::with_capacity(n + 1);
    let mut carry = 0u8;
    for i in 0..n {
        let x = a.get(i).copied().unwrap_or(0) + b.get(i).copied().unwrap_or(0) + carry;
        out.push(b'0' + x % 10);
        carry = x / 10;
    }
    if carry > 0 {
        out.push(b'0' + carry);
    }
    out.reverse();
    String::from_utf8(out).unwrap_or_else(|_| "0".to_string())
}

/// Schoolbook subtraction `a - b` of two non-negative integer digit strings,
/// assuming `a >= b`.
fn sub_digit_strings(a: &str, b: &str) -> String {
    let a: Vec<i16> = a.bytes().rev().map(|c| (c - b'0') as i16).collect();
    let b: Vec<i16> = b.bytes().rev().map(|c| (c - b'0') as i16).collect();
    let mut out = Vec::with_capacity(a.len());
    let mut borrow = 0i16;
    for (i, &ad) in a.iter().enumerate() {
        let mut x = ad - borrow - b.get(i).copied().unwrap_or(0);
        if x < 0 {
            x += 10;
            borrow = 1;
        } else {
            borrow = 0;
        }
        out.push(b'0' + x as u8);
    }
    while out.len() > 1 && *out.last().unwrap() == b'0' {
        out.pop();
    }
    out.reverse();
    String::from_utf8(out).unwrap_or_else(|_| "0".to_string())
}

/// Render a signed `(int, frac)` magnitude as a DynamoDB Number string with
/// insignificant zeros stripped and no trailing `.0` for integral results.
fn format_decimal(neg: bool, int: &str, frac: &str) -> String {
    let int_t = int.trim_start_matches('0');
    let frac_t = frac.trim_end_matches('0');
    let is_zero = int_t.is_empty() && frac_t.is_empty();
    let sign = if neg && !is_zero { "-" } else { "" };
    let int_disp = if int_t.is_empty() { "0" } else { int_t };
    if frac_t.is_empty() {
        format!("{sign}{int_disp}")
    } else {
        format!("{sign}{int_disp}.{frac_t}")
    }
}

/// Two AttributeValues are comparable by the relational operators (`<`, `<=`,
/// `>`, `>=`, BETWEEN) only when they share a scalar type (S, N, or B).
/// DynamoDB does not match a comparison across mismatched types; without this
/// guard, `compare_attribute_values` falls back to `Equal` for a type mismatch,
/// which wrongly satisfies `<=`/`>=`/BETWEEN.
pub(crate) fn comparable_types(a: Option<&Value>, b: Option<&Value>) -> bool {
    match (
        a.and_then(attribute_type_and_value),
        b.and_then(attribute_type_and_value),
    ) {
        (Some((ta, _)), Some((tb, _))) => ta == tb && matches!(ta, "S" | "N" | "B"),
        _ => false,
    }
}

pub(crate) fn compare_attribute_values(a: Option<&Value>, b: Option<&Value>) -> std::cmp::Ordering {
    match (a, b) {
        (None, None) => std::cmp::Ordering::Equal,
        (None, Some(_)) => std::cmp::Ordering::Less,
        (Some(_), None) => std::cmp::Ordering::Greater,
        (Some(a), Some(b)) => {
            let a_type = attribute_type_and_value(a);
            let b_type = attribute_type_and_value(b);
            match (a_type, b_type) {
                (Some(("S", a_val)), Some(("S", b_val))) => {
                    let a_str = a_val.as_str().unwrap_or("");
                    let b_str = b_val.as_str().unwrap_or("");
                    a_str.cmp(b_str)
                }
                (Some(("N", a_val)), Some(("N", b_val))) => {
                    // DynamoDB numbers are arbitrary-precision decimals (up
                    // to 38 significant digits). Parsing to f64 silently
                    // rounds past 2^53, so two distinct large integers
                    // compare Equal and sort wrong (bug-audit 2026-05-28,
                    // 1.11). Compare the decimal strings directly.
                    let a_str = a_val.as_str().unwrap_or("0");
                    let b_str = b_val.as_str().unwrap_or("0");
                    compare_number_strings(a_str, b_str)
                }
                (Some(("B", a_val)), Some(("B", b_val))) => {
                    // Binary compares as unsigned bytes, not as its base64
                    // text, whose order differs ("/w==" is 0xff but sorts
                    // before "AAE=", 0x00 0x01).
                    use base64::Engine;
                    let a_str = a_val.as_str().unwrap_or("");
                    let b_str = b_val.as_str().unwrap_or("");
                    let decode = |s: &str| base64::engine::general_purpose::STANDARD.decode(s);
                    match (decode(a_str), decode(b_str)) {
                        (Ok(a_bytes), Ok(b_bytes)) => a_bytes.cmp(&b_bytes),
                        _ => a_str.cmp(b_str),
                    }
                }
                _ => std::cmp::Ordering::Equal,
            }
        }
    }
}

/// Equality for `=` / `<>` that is numeric-aware: two N values are equal when
/// they are the same decimal regardless of formatting (`3.10` == `3.1`), while
/// every other type falls back to exact JSON equality (so Maps/Lists/Bool/sets
/// keep strict equality). Plain `a == b` on the raw JSON wrongly treated
/// decimally-equal numbers as unequal (bug-audit 2026-06-26, 1.16).
pub(crate) fn values_equal(a: Option<&Value>, b: Option<&Value>) -> bool {
    match (a, b) {
        (Some(av), Some(bv)) => {
            if let (Some(("N", an)), Some(("N", bn))) =
                (attribute_type_and_value(av), attribute_type_and_value(bv))
            {
                let (an, bn) = (
                    an.as_str().unwrap_or_default(),
                    bn.as_str().unwrap_or_default(),
                );
                // Only canonicalize when BOTH are valid numbers. A malformed
                // operand ({"N":"abc"}) must not compare equal to a valid stored
                // key, so fall back to strict byte-equality there -- otherwise
                // DeleteItem could delete the wrong row (Cubic P1, 2026-07-01).
                if is_valid_number(an) && is_valid_number(bn) {
                    compare_number_strings(an, bn) == std::cmp::Ordering::Equal
                } else {
                    av == bv
                }
            } else {
                av == bv
            }
        }
        (None, None) => true,
        _ => false,
    }
}

/// Whether two Number-set (`NS`) members are the same member. AWS compares NS
/// members by numeric value, so `"1"` and `"1.0"` are one member: a set can
/// never hold both, `ADD` dedups by value, and `DELETE` removes by value. The
/// members are the bare decimal strings stored inside the `NS` array. Malformed
/// members fall back to exact string equality so a bad operand can never be
/// mistaken for a valid stored member. Mirrors the read side's `values_equal`.
pub(crate) fn ns_members_equal(a: &Value, b: &Value) -> bool {
    match (a.as_str(), b.as_str()) {
        (Some(x), Some(y)) if is_valid_number(x) && is_valid_number(y) => {
            compare_number_strings(x, y) == std::cmp::Ordering::Equal
        }
        _ => a == b,
    }
}

/// Match a string against a PartiQL/SQL LIKE pattern. `%` matches any
/// run of characters (including empty), `_` matches exactly one
/// character. Both wildcards are anchored — `LIKE 'foo'` requires an
/// exact match, mirroring DDB PartiQL semantics.
pub(crate) fn match_like(s: &str, pattern: &str) -> bool {
    let s_chars: Vec<char> = s.chars().collect();
    let p_chars: Vec<char> = pattern.chars().collect();
    like_recurse(&s_chars, 0, &p_chars, 0)
}

fn like_recurse(s: &[char], si: usize, p: &[char], pi: usize) -> bool {
    if pi == p.len() {
        return si == s.len();
    }
    match p[pi] {
        '%' => {
            // Greedy backtracking: try matching 0..=remaining chars.
            for k in si..=s.len() {
                if like_recurse(s, k, p, pi + 1) {
                    return true;
                }
            }
            false
        }
        '_' => si < s.len() && like_recurse(s, si + 1, p, pi + 1),
        c => si < s.len() && s[si] == c && like_recurse(s, si + 1, p, pi + 1),
    }
}

/// Validate that every key-schema attribute is present in the item AND
/// that its AttributeValue carries the declared scalar type from
/// `attribute_definitions`. Real DDB rejects an INSERT or PutItem that
/// omits a key or supplies the wrong type with a `ValidationException`;
/// without the type check we'd silently accept e.g. `{'pk': 1}` for a
/// HASH key declared as `S`.
pub(crate) fn validate_partiql_item_against_key_schema(
    table: &DynamoTable,
    item: &HashMap<String, AttributeValue>,
) -> Result<(), AwsServiceError> {
    for key_attr in &table.key_schema {
        let Some(val) = item.get(&key_attr.attribute_name) else {
            return Err(AwsServiceError::aws_error(
                StatusCode::BAD_REQUEST,
                "ValidationException",
                format!(
                    "One or more parameter values were invalid: Missing the key {} in the item",
                    key_attr.attribute_name
                ),
            ));
        };
        // Type check against AttributeDefinitions. AWS only allows
        // S/N/B for key attribute types.
        let declared = table
            .attribute_definitions
            .iter()
            .find(|d| d.attribute_name == key_attr.attribute_name)
            .map(|d| d.attribute_type.as_str());
        if let Some(expected) = declared {
            let obj = val.as_object();
            let actual_tag = obj.and_then(|o| o.keys().next().map(|k| k.as_str()));
            if actual_tag != Some(expected) {
                return Err(AwsServiceError::aws_error(
                    StatusCode::BAD_REQUEST,
                    "ValidationException",
                    format!(
                        "One or more parameter values were invalid: Type mismatch for key {} expected: {} actual: {}",
                        key_attr.attribute_name,
                        expected,
                        actual_tag.unwrap_or("?"),
                    ),
                ));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod number_compare_tests {
    use super::*;
    use std::cmp::Ordering;

    // bug-audit 2026-05-28, 1.11: integers past 2^53 must compare exactly,
    // not get rounded to the same f64.
    #[test]
    fn large_integers_compare_exactly() {
        assert_eq!(
            compare_number_strings("9007199254740993", "9007199254740992"),
            Ordering::Greater
        );
        assert_eq!(
            compare_number_strings("9007199254740992", "9007199254740993"),
            Ordering::Less
        );
        assert_eq!(
            compare_number_strings("12345678901234567890", "12345678901234567890"),
            Ordering::Equal
        );
    }

    #[test]
    fn signs_decimals_and_exponents() {
        assert_eq!(compare_number_strings("-5", "3"), Ordering::Less);
        assert_eq!(compare_number_strings("-5", "-3"), Ordering::Less);
        assert_eq!(compare_number_strings("3.14", "3.2"), Ordering::Less);
        assert_eq!(compare_number_strings("3.10", "3.1"), Ordering::Equal);
        assert_eq!(compare_number_strings("0", "-0"), Ordering::Equal);
        assert_eq!(compare_number_strings("10", "9"), Ordering::Greater);
        assert_eq!(compare_number_strings("1e3", "1000"), Ordering::Equal);
        assert_eq!(compare_number_strings("1e-2", "0.01"), Ordering::Equal);
    }

    // A negative value must always sort below a non-negative one regardless of
    // magnitude; the cross-sign arms must not be swapped.
    #[test]
    fn cross_sign_ordering_is_directional() {
        assert_eq!(compare_number_strings("-1", "1"), Ordering::Less);
        assert_eq!(compare_number_strings("1", "-1"), Ordering::Greater);
        // A small negative still loses to a large positive, and vice versa.
        assert_eq!(compare_number_strings("-100", "1"), Ordering::Less);
        assert_eq!(compare_number_strings("100", "-1"), Ordering::Greater);
        // Negative zero is normalized, so it ties zero rather than sorting low.
        assert_eq!(compare_number_strings("-0", "5"), Ordering::Less);
        assert_eq!(compare_number_strings("-0", "0"), Ordering::Equal);
        // Both negative: larger magnitude is the smaller value.
        assert_eq!(compare_number_strings("-100", "-1"), Ordering::Less);
        assert_eq!(compare_number_strings("-1", "-100"), Ordering::Greater);
    }

    // bug-audit 2026-06-28: SET/ADD arithmetic must be arbitrary precision.
    // f64 rounds past 2^53 and `as i64` saturates past ~9.2e18, corrupting
    // large counters; decimal_add_sub works on the digit strings instead.
    #[test]
    fn decimal_add_sub_big_integers_exact() {
        // Far beyond 2^53 (9007199254740992) and i64::MAX (9223372036854775807).
        assert_eq!(
            decimal_add_sub("9007199254740992", "1", true).unwrap(),
            "9007199254740993"
        );
        assert_eq!(
            decimal_add_sub("99999999999999999999999999999999999999", "1", true).unwrap(),
            "100000000000000000000000000000000000000"
        );
        assert_eq!(
            decimal_add_sub("9223372036854775807", "9223372036854775807", true).unwrap(),
            "18446744073709551614"
        );
    }

    #[test]
    fn decimal_add_sub_signs_and_fractions() {
        assert_eq!(decimal_add_sub("5", "3", false).unwrap(), "2");
        assert_eq!(decimal_add_sub("3", "5", false).unwrap(), "-2");
        assert_eq!(decimal_add_sub("-5", "3", true).unwrap(), "-2");
        assert_eq!(decimal_add_sub("5", "-3", true).unwrap(), "2");
        assert_eq!(decimal_add_sub("0.1", "0.2", true).unwrap(), "0.3");
        assert_eq!(decimal_add_sub("1.5", "2.5", true).unwrap(), "4");
        assert_eq!(decimal_add_sub("10", "10", false).unwrap(), "0");
        assert_eq!(decimal_add_sub("100.25", "0.75", true).unwrap(), "101");
        // Carry/borrow across the decimal point.
        assert_eq!(decimal_add_sub("1", "0.001", false).unwrap(), "0.999");
        // Non-numeric operand is rejected.
        assert!(decimal_add_sub("abc", "1", true).is_none());
    }
}
