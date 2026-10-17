//! `InvokeGuardrailChecks` (bedrock-runtime) — the inline guardrail-tier API
//! that evaluates a set of messages against requested checks without a
//! pre-created guardrail. Real (not stubbed) evaluation: content-filter and
//! prompt-attack categories are matched against keyword/pattern sets, and
//! sensitive-information entities are detected with regexes that report the
//! match offsets, mirroring the response shape AWS returns.

use std::sync::OnceLock;

use fakecloud_core::service::{AwsResponse, AwsServiceError};
use regex::Regex;
use serde_json::{json, Value};

use crate::runtime_validation::{validate_enum, validate_length, validation};

/// One message content block, flattened with its position for offset reporting.
struct TextBlock {
    message_index: usize,
    content_index: usize,
    text: String,
}

fn collect_text_blocks(messages: &Value) -> Vec<TextBlock> {
    let mut blocks = Vec::new();
    for (mi, msg) in messages.as_array().into_iter().flatten().enumerate() {
        for (ci, c) in msg["content"].as_array().into_iter().flatten().enumerate() {
            if let Some(t) = c["text"].as_str() {
                blocks.push(TextBlock {
                    message_index: mi,
                    content_index: ci,
                    text: t.to_string(),
                });
            }
        }
    }
    blocks
}

/// AWS bills guardrail text in 1000-character "text units" (min 1).
fn text_units(blocks: &[TextBlock]) -> i64 {
    let chars: i64 = blocks.iter().map(|b| b.text.chars().count() as i64).sum();
    ((chars + 999) / 1000).max(1)
}

/// Lowercased keyword sets per content-filter category.
fn content_filter_keywords(category: &str) -> &'static [&'static str] {
    match category {
        "VIOLENCE" => &[
            "kill", "attack", "bomb", "murder", "assault", "shoot", "weapon",
        ],
        "HATE" => &["hate", "racist", "bigot", "slur"],
        "SEXUAL" => &["sex", "nude", "porn", "explicit"],
        "MISCONDUCT" => &["hack", "steal", "illegal", "fraud", "launder", "exploit"],
        "INSULTS" => &["idiot", "stupid", "dumb", "loser", "moron"],
        _ => &[],
    }
}

/// Lowercased phrase sets per prompt-attack category.
fn prompt_attack_phrases(category: &str) -> &'static [&'static str] {
    match category {
        "JAILBREAK" => &[
            "ignore previous instructions",
            "ignore all previous",
            "do anything now",
            "dan mode",
            "jailbreak",
            "without any restrictions",
        ],
        "PROMPT_INJECTION" => &[
            "ignore the above",
            "disregard the previous",
            "new instructions:",
            "override your",
        ],
        "PROMPT_LEAKAGE" => &[
            "repeat your system prompt",
            "what is your system prompt",
            "reveal your instructions",
            "print your instructions",
        ],
        _ => &[],
    }
}

/// Luhn checksum over the digits of a candidate card number, so arbitrary
/// 13-19 digit runs (order numbers, timestamps) aren't reported as cards.
fn luhn_valid(candidate: &str) -> bool {
    let digits: Vec<u32> = candidate.chars().filter_map(|c| c.to_digit(10)).collect();
    if !(13..=19).contains(&digits.len()) {
        return false;
    }
    let sum: u32 = digits
        .iter()
        .rev()
        .enumerate()
        .map(|(i, &d)| {
            if i % 2 == 1 {
                let x = d * 2;
                if x > 9 {
                    x - 9
                } else {
                    x
                }
            } else {
                d
            }
        })
        .sum();
    sum.is_multiple_of(10)
}

/// Regex for a sensitive-information entity type, or `None` when the type is
/// one AWS detects contextually (names, addresses, ages, usernames, ...) and
/// no structural pattern can find it without false hits.
fn pii_regex(entity: &str) -> Option<&'static Regex> {
    macro_rules! re {
        ($pat:expr) => {{
            static R: OnceLock<Regex> = OnceLock::new();
            Some(R.get_or_init(|| Regex::new($pat).expect("static PII regex")))
        }};
    }
    match entity {
        "EMAIL" => re!(r"[a-zA-Z0-9._%+-]+@[a-zA-Z0-9.-]+\.[a-zA-Z]{2,}"),
        // International / free-form numbers (`+1 (555) 123-4567`,
        // `+44 20 7946 0958`) or the compact US `555.123.4567` form.
        "PHONE" => re!(r"\+?\b\d[\d\-() ]{7,}\d\b|\b\d{3}[-.]?\d{3}[-.]?\d{4}\b"),
        "US_SOCIAL_SECURITY_NUMBER" => re!(r"\b\d{3}-\d{2}-\d{4}\b"),
        "US_INDIVIDUAL_TAX_IDENTIFICATION_NUMBER" => {
            re!(r"\b9\d{2}-(?:5\d|6[0-5]|7\d|8[0-8]|9[0-24-9])-\d{4}\b")
        }
        // 13-19 digits, contiguous or in any space/dash grouping (4-4-4-4,
        // Amex 4-6-5, Diners 4-6-4, 4-4-4-4-3, ...); Luhn-filtered in
        // `pii_matches`.
        "CREDIT_DEBIT_CARD_NUMBER" => re!(r"\b\d(?:[ -]?\d){12,18}\b"),
        "CREDIT_DEBIT_CARD_EXPIRY" => re!(r"\b(?:0[1-9]|1[0-2])/(?:\d{4}|\d{2})\b"),
        "IP_ADDRESS" => re!(r"\b\d{1,3}\.\d{1,3}\.\d{1,3}\.\d{1,3}\b"),
        "MAC_ADDRESS" => re!(r"\b[0-9A-Fa-f]{2}(?:[:-][0-9A-Fa-f]{2}){5}\b"),
        "AWS_ACCESS_KEY" => re!(r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b"),
        "URL" => re!(r"https?://[^\s]+"),
        "INTERNATIONAL_BANK_ACCOUNT_NUMBER" => re!(r"\b[A-Z]{2}\d{2}[A-Z0-9]{11,30}\b"),
        "UK_NATIONAL_INSURANCE_NUMBER" => {
            re!(r"\b[A-CEGHJ-PR-TW-Z]{2} ?\d{2} ?\d{2} ?\d{2} ?[A-D]\b")
        }
        "CA_SOCIAL_INSURANCE_NUMBER" => re!(r"\b\d{3}-\d{3}-\d{3}\b"),
        "VEHICLE_IDENTIFICATION_NUMBER" => re!(r"\b[A-HJ-NPR-Z0-9]{17}\b"),
        _ => None,
    }
}

/// Byte ranges of every `entity` detection in `text`, in ascending order.
/// Empty when the type has no structural detector.
///
/// Shared by `InvokeGuardrailChecks` and `ApplyGuardrail`'s
/// `piiEntitiesConfig` evaluation so both APIs detect the same entities.
pub(crate) fn pii_matches(entity: &str, text: &str) -> Vec<std::ops::Range<usize>> {
    let Some(re) = pii_regex(entity) else {
        return Vec::new();
    };
    re.find_iter(text)
        .filter(|m| match entity {
            "CREDIT_DEBIT_CARD_NUMBER" => luhn_valid(m.as_str()),
            "PHONE" => plausible_phone(m.as_str()),
            _ => true,
        })
        .map(|m| m.range())
        .collect()
}

/// The permissive PHONE pattern also matches other digit groupings. An E.164
/// number has at most 15 digits, and a US SSN (`ddd-dd-dddd`) is its own
/// entity type, so neither is reported as a phone number.
fn plausible_phone(s: &str) -> bool {
    let digits = s.chars().filter(char::is_ascii_digit).count();
    let ssn_shaped = s.len() == 11
        && s.char_indices().all(|(i, c)| match i {
            3 | 6 => c == '-',
            _ => c.is_ascii_digit(),
        });
    digits <= 15 && !ssn_shaped
}

/// `GuardrailChecksRole` enum values.
const ROLES: &[&str] = &["user", "assistant", "system"];
const CONTENT_FILTER_CATEGORIES: &[&str] = &["VIOLENCE", "HATE", "SEXUAL", "MISCONDUCT", "INSULTS"];
const PROMPT_ATTACK_CATEGORIES: &[&str] = &["JAILBREAK", "PROMPT_INJECTION", "PROMPT_LEAKAGE"];
/// `GuardrailChecksSensitiveInformationEntityType` enum values.
const SENSITIVE_ENTITY_TYPES: &[&str] = &[
    "ADDRESS",
    "AGE",
    "AWS_ACCESS_KEY",
    "AWS_SECRET_KEY",
    "CA_HEALTH_NUMBER",
    "CA_SOCIAL_INSURANCE_NUMBER",
    "CREDIT_DEBIT_CARD_CVV",
    "CREDIT_DEBIT_CARD_EXPIRY",
    "CREDIT_DEBIT_CARD_NUMBER",
    "DRIVER_ID",
    "EMAIL",
    "INTERNATIONAL_BANK_ACCOUNT_NUMBER",
    "IP_ADDRESS",
    "LICENSE_PLATE",
    "MAC_ADDRESS",
    "NAME",
    "PASSWORD",
    "PHONE",
    "PIN",
    "SWIFT_CODE",
    "UK_NATIONAL_HEALTH_SERVICE_NUMBER",
    "UK_NATIONAL_INSURANCE_NUMBER",
    "UK_UNIQUE_TAXPAYER_REFERENCE_NUMBER",
    "URL",
    "USERNAME",
    "US_BANK_ACCOUNT_NUMBER",
    "US_BANK_ROUTING_NUMBER",
    "US_INDIVIDUAL_TAX_IDENTIFICATION_NUMBER",
    "US_PASSPORT_NUMBER",
    "US_SOCIAL_SECURITY_NUMBER",
    "VEHICLE_IDENTIFICATION_NUMBER",
];

fn missing(field: &str) -> AwsServiceError {
    validation(format!(
        "Value null at '{field}' failed to satisfy constraint: Member must not be null"
    ))
}

/// Validate a `@length`-bounded list member, returning its elements.
fn list_member<'a>(
    parent: &'a Value,
    key: &str,
    path: &str,
    min: usize,
    max: Option<usize>,
) -> Result<&'a Vec<Value>, AwsServiceError> {
    let list = match parent.get(key) {
        None | Some(Value::Null) => return Err(missing(path)),
        Some(Value::Array(a)) => a,
        Some(_) => return Err(validation(format!("Value at '{path}' must be a list"))),
    };
    if list.len() < min {
        return Err(validation(format!(
            "Value at '{path}' failed to satisfy constraint: Member must have length greater than or equal to {min}"
        )));
    }
    if let Some(max) = max.filter(|m| list.len() > *m) {
        return Err(validation(format!(
            "Value at '{path}' failed to satisfy constraint: Member must have length less than or equal to {max}"
        )));
    }
    Ok(list)
}

/// Validate a required enum-string member.
fn enum_member(
    parent: &Value,
    key: &str,
    path: &str,
    allowed: &[&str],
) -> Result<String, AwsServiceError> {
    match parent.get(key) {
        None | Some(Value::Null) => Err(missing(path)),
        Some(Value::String(v)) => validate_enum(path, v, allowed).map(|()| v.clone()),
        Some(_) => Err(validation(format!("Value at '{path}' must be a string"))),
    }
}

/// Validate the `messages` list per `GuardrailChecksMessageList`.
fn validate_messages(body: &Value) -> Result<(), AwsServiceError> {
    let messages = list_member(body, "messages", "messages", 1, None)?;
    for (mi, msg) in messages.iter().enumerate() {
        let path = format!("messages.{}", mi + 1);
        enum_member(msg, "role", &format!("{path}.member.role"), ROLES)?;
        let content = list_member(msg, "content", &format!("{path}.member.content"), 1, None)?;
        for (ci, block) in content.iter().enumerate() {
            let bpath = format!("{path}.member.content.{}", ci + 1);
            // `GuardrailChecksContentBlock` is a union whose only member is
            // `text` (`GuardrailChecksTextContent`, @length min 1). Members
            // sent as explicit JSON `null` are unset, so only non-null
            // members count toward "exactly one".
            let set: Vec<(&String, &Value)> = block
                .as_object()
                .into_iter()
                .flatten()
                .filter(|(_, v)| !v.is_null())
                .collect();
            match set.as_slice() {
                [] => return Err(missing(&bpath)),
                [(name, Value::String(t))] if name.as_str() == "text" => {
                    validate_length(&format!("{bpath}.member.text"), t, 1, usize::MAX)?
                }
                _ => {
                    return Err(validation(format!(
                        "Value at '{bpath}' failed to satisfy constraint: Member must have exactly one of the union members [text] set"
                    )))
                }
            }
        }
    }
    Ok(())
}

/// Validate the `checks` structure, returning the requested category /
/// entity names for each configured check.
struct RequestedChecks {
    content_filter: Option<Vec<String>>,
    prompt_attack: Option<Vec<String>>,
    sensitive_information: Option<Vec<String>>,
}

fn validate_checks(body: &Value) -> Result<RequestedChecks, AwsServiceError> {
    let checks = match body.get("checks") {
        None | Some(Value::Null) => return Err(missing("checks")),
        Some(c @ Value::Object(_)) => c,
        Some(_) => return Err(validation("Value at 'checks' must be a structure")),
    };
    let family = |key: &str,
                  list_key: &str,
                  field: &str,
                  max: usize,
                  allowed: &[&str]|
     -> Result<Option<Vec<String>>, AwsServiceError> {
        let Some(cfg) = checks.get(key).filter(|v| !v.is_null()) else {
            return Ok(None);
        };
        let path = format!("checks.{key}.{list_key}");
        let entries = list_member(cfg, list_key, &path, 1, Some(max))?;
        entries
            .iter()
            .enumerate()
            .map(|(i, e)| {
                enum_member(
                    e,
                    field,
                    &format!("{path}.{}.member.{field}", i + 1),
                    allowed,
                )
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Some)
    };
    Ok(RequestedChecks {
        content_filter: family(
            "contentFilter",
            "categories",
            "category",
            5,
            CONTENT_FILTER_CATEGORIES,
        )?,
        prompt_attack: family(
            "promptAttack",
            "categories",
            "category",
            3,
            PROMPT_ATTACK_CATEGORIES,
        )?,
        sensitive_information: family(
            "sensitiveInformation",
            "entities",
            "type",
            31,
            SENSITIVE_ENTITY_TYPES,
        )?,
    })
}

/// Deduplicate requested names while keeping first-seen order, so a category
/// listed twice is evaluated (and reported) once.
fn dedup(names: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    names
        .into_iter()
        .filter(|n| seen.insert(n.clone()))
        .collect()
}

pub fn invoke_guardrail_checks(body: &Value) -> Result<AwsResponse, AwsServiceError> {
    validate_messages(body)?;
    let requested = validate_checks(body)?;

    let blocks = collect_text_blocks(&body["messages"]);
    let units = text_units(&blocks);
    let lower: Vec<String> = blocks.iter().map(|b| b.text.to_lowercase()).collect();

    let mut results = serde_json::Map::new();
    let mut usage = serde_json::Map::new();

    // ── content filter ──
    if let Some(categories) = requested.content_filter {
        let mut entries = Vec::new();
        for category in dedup(categories) {
            let kws = content_filter_keywords(&category);
            if lower.iter().any(|t| kws.iter().any(|k| t.contains(k))) {
                entries.push(json!({ "category": category, "severityScore": 0.85 }));
            }
        }
        results.insert("contentFilter".into(), json!({ "results": entries }));
        usage.insert("contentFilter".into(), json!({ "textUnits": units }));
    }

    // ── prompt attack ──
    if let Some(categories) = requested.prompt_attack {
        let mut entries = Vec::new();
        for category in dedup(categories) {
            let phrases = prompt_attack_phrases(&category);
            if lower.iter().any(|t| phrases.iter().any(|p| t.contains(p))) {
                entries.push(json!({ "category": category, "severityScore": 0.9 }));
            }
        }
        results.insert("promptAttack".into(), json!({ "results": entries }));
        usage.insert("promptAttack".into(), json!({ "textUnits": units }));
    }

    // ── sensitive information ──
    if let Some(entities) = requested.sensitive_information {
        let entities = dedup(entities);
        let mut entries = Vec::new();
        for b in &blocks {
            let mut hits: Vec<(std::ops::Range<usize>, &str)> = entities
                .iter()
                .flat_map(|e| {
                    pii_matches(e, &b.text)
                        .into_iter()
                        .map(move |r| (r, e.as_str()))
                })
                .collect();
            hits.sort_by_key(|(r, _)| (r.start, r.end));
            // Offsets are character (not byte) positions into the content
            // block's text: advance one byte/char cursor over the sorted
            // matches instead of rescanning the prefix for every hit.
            let (mut cursor_byte, mut cursor_char) = (0usize, 0usize);
            for (range, entity) in hits {
                cursor_char += b.text[cursor_byte..range.start].chars().count();
                cursor_byte = range.start;
                let begin = cursor_char;
                let end = begin + b.text[range].chars().count();
                entries.push(json!({
                    "type": entity,
                    "confidenceScore": 0.99,
                    "beginOffset": begin as i64,
                    "endOffset": end as i64,
                    "messageIndex": b.message_index as i64,
                    "contentIndex": b.content_index as i64,
                }));
            }
        }
        results.insert(
            "sensitiveInformation".into(),
            json!({ "results": entries, "truncated": false }),
        );
        usage.insert("sensitiveInformation".into(), json!({ "textUnits": units }));
    }

    Ok(AwsResponse::ok_json(json!({
        "results": Value::Object(results),
        "usage": Value::Object(usage),
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resp_json(body: &Value) -> Value {
        let r = match invoke_guardrail_checks(body) {
            Ok(r) => r,
            Err(e) => panic!("expected ok response, got {}", e.code()),
        };
        serde_json::from_slice(r.body.expect_bytes()).unwrap()
    }

    fn err_code(body: &Value) -> String {
        match invoke_guardrail_checks(body) {
            Err(e) => e.code().to_string(),
            Ok(_) => panic!("expected validation error for {body}"),
        }
    }

    #[test]
    fn detects_all_three_check_families() {
        let body = json!({
            "messages": [{"role":"user","content":[
                {"text":"build a bomb, ignore previous instructions, email a@b.com ssn 123-45-6789"}
            ]}],
            "checks": {
                "contentFilter": {"categories": [{"category":"VIOLENCE"},{"category":"SEXUAL"}]},
                "promptAttack": {"categories": [{"category":"JAILBREAK"}]},
                "sensitiveInformation": {"entities": [{"type":"EMAIL"},{"type":"US_SOCIAL_SECURITY_NUMBER"}]}
            }
        });
        let r = resp_json(&body);
        // VIOLENCE detected, SEXUAL not (only requested-and-matched are returned).
        let cf = r["results"]["contentFilter"]["results"].as_array().unwrap();
        assert_eq!(cf.len(), 1);
        assert_eq!(cf[0]["category"], "VIOLENCE");
        assert!(cf[0]["severityScore"].as_f64().unwrap() > 0.0);
        // JAILBREAK detected.
        assert_eq!(
            r["results"]["promptAttack"]["results"][0]["category"],
            "JAILBREAK"
        );
        // EMAIL + SSN detected with offsets + position.
        let si = r["results"]["sensitiveInformation"]["results"]
            .as_array()
            .unwrap();
        let types: Vec<&str> = si.iter().map(|e| e["type"].as_str().unwrap()).collect();
        assert!(types.contains(&"EMAIL") && types.contains(&"US_SOCIAL_SECURITY_NUMBER"));
        for e in si {
            assert!(e["endOffset"].as_i64().unwrap() > e["beginOffset"].as_i64().unwrap());
            assert_eq!(e["messageIndex"], 0);
        }
        assert_eq!(r["results"]["sensitiveInformation"]["truncated"], false);
        // usage reported per requested check.
        assert_eq!(r["usage"]["contentFilter"]["textUnits"], 1);
        assert_eq!(r["usage"]["promptAttack"]["textUnits"], 1);
        assert_eq!(r["usage"]["sensitiveInformation"]["textUnits"], 1);
    }

    #[test]
    fn clean_text_yields_no_detections() {
        let body = json!({
            "messages": [{"role":"user","content":[{"text":"what a lovely sunny day"}]}],
            "checks": {"contentFilter": {"categories": [{"category":"VIOLENCE"}]}}
        });
        let r = resp_json(&body);
        assert!(r["results"]["contentFilter"]["results"]
            .as_array()
            .unwrap()
            .is_empty());
        // Only the requested check appears.
        assert!(r["results"]["promptAttack"].is_null());
        assert!(r["usage"]["promptAttack"].is_null());
    }

    #[test]
    fn offsets_locate_the_match_in_its_own_block() {
        let body = json!({
            "messages": [
                {"role":"system","content":[{"text":"nothing here"}]},
                {"role":"user","content":[
                    {"text":"plain"},
                    {"text":"héllo, mail me at x@y.io"}
                ]}
            ],
            "checks": {"sensitiveInformation": {"entities": [{"type":"EMAIL"}]}}
        });
        let r = resp_json(&body);
        let e = &r["results"]["sensitiveInformation"]["results"][0];
        assert_eq!(e["messageIndex"], 1);
        assert_eq!(e["contentIndex"], 1);
        // Character offsets: "héllo, mail me at " is 18 chars ("é" is one).
        assert_eq!(e["beginOffset"], 18);
        assert_eq!(e["endOffset"], 24);
    }

    #[test]
    fn duplicate_categories_are_reported_once() {
        let body = json!({
            "messages": [{"role":"user","content":[{"text":"you idiot"}]}],
            "checks": {"contentFilter": {"categories": [
                {"category":"INSULTS"},{"category":"INSULTS"}
            ]}}
        });
        let r = resp_json(&body);
        assert_eq!(
            r["results"]["contentFilter"]["results"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn text_units_bill_per_thousand_chars() {
        let body = json!({
            "messages": [{"role":"user","content":[{"text": "a".repeat(2500)}]}],
            "checks": {"promptAttack": {"categories": [{"category":"JAILBREAK"}]}}
        });
        assert_eq!(resp_json(&body)["usage"]["promptAttack"]["textUnits"], 3);
    }

    #[test]
    fn structural_pii_entities_are_detected() {
        for (entity, text) in [
            ("MAC_ADDRESS", "nic 00:1A:2b:3C:4d:5E up"),
            ("CREDIT_DEBIT_CARD_EXPIRY", "expires 09/27 ok"),
            ("AWS_ACCESS_KEY", "key AKIAIOSFODNN7EXAMPLE here"),
            ("UK_NATIONAL_INSURANCE_NUMBER", "ni AB 12 34 56 C"),
            (
                "INTERNATIONAL_BANK_ACCOUNT_NUMBER",
                "iban GB82WEST12345698765432",
            ),
        ] {
            let body = json!({
                "messages": [{"role":"user","content":[{"text": text}]}],
                "checks": {"sensitiveInformation": {"entities": [{"type": entity}]}}
            });
            let r = resp_json(&body);
            assert_eq!(
                r["results"]["sensitiveInformation"]["results"][0]["type"], entity,
                "{entity}"
            );
        }
    }

    fn detected(entity: &str, text: &str) -> Vec<String> {
        let body = json!({
            "messages": [{"role":"user","content":[{"text": text}]}],
            "checks": {"sensitiveInformation": {"entities": [{"type": entity}]}}
        });
        let r = resp_json(&body);
        r["results"]["sensitiveInformation"]["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| {
                let (b, en) = (
                    e["beginOffset"].as_u64().unwrap() as usize,
                    e["endOffset"].as_u64().unwrap() as usize,
                );
                text.chars().skip(b).take(en - b).collect()
            })
            .collect()
    }

    #[test]
    fn phone_detects_international_formats() {
        assert_eq!(
            detected("PHONE", "ring +1 (555) 123-4567 today"),
            vec!["+1 (555) 123-4567"]
        );
        assert_eq!(
            detected("PHONE", "london +44 20 7946 0958."),
            vec!["+44 20 7946 0958"]
        );
        assert_eq!(detected("PHONE", "dot 555.123.4567"), vec!["555.123.4567"]);
    }

    #[test]
    fn phone_skips_ssn_and_card_shaped_numbers() {
        assert!(detected("PHONE", "ssn 123-45-6789").is_empty());
        assert!(detected("PHONE", "card 4111 1111 1111 1111").is_empty());
        assert_eq!(
            pii_matches("PHONE", "ssn 123-45-6789 or call 555-123-4567").len(),
            1
        );
    }

    #[test]
    fn card_numbers_13_to_19_digits_luhn_checked() {
        assert_eq!(
            detected("CREDIT_DEBIT_CARD_NUMBER", "amex 3782 822463 10005 ok"),
            vec!["3782 822463 10005"]
        );
        assert_eq!(
            detected("CREDIT_DEBIT_CARD_NUMBER", "visa 4222222222222 ok"),
            vec!["4222222222222"]
        );
        assert_eq!(
            detected("CREDIT_DEBIT_CARD_NUMBER", "card 4111 1111 1111 1111"),
            vec!["4111 1111 1111 1111"]
        );
        assert!(detected("CREDIT_DEBIT_CARD_NUMBER", "order 1234567890123").is_empty());
    }

    #[test]
    fn offsets_are_ordered_and_char_based_across_entities() {
        let text = "é a@b.co then 123-45-6789 and c@d.io";
        let body = json!({
            "messages": [{"role":"user","content":[{"text": text}]}],
            "checks": {"sensitiveInformation": {"entities": [
                {"type":"US_SOCIAL_SECURITY_NUMBER"},{"type":"EMAIL"}
            ]}}
        });
        let r = resp_json(&body);
        let got: Vec<(String, i64, i64)> = r["results"]["sensitiveInformation"]["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| {
                (
                    e["type"].as_str().unwrap().to_string(),
                    e["beginOffset"].as_i64().unwrap(),
                    e["endOffset"].as_i64().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            got,
            vec![
                ("EMAIL".to_string(), 2, 8),
                ("US_SOCIAL_SECURITY_NUMBER".to_string(), 14, 25),
                ("EMAIL".to_string(), 30, 36),
            ]
        );
    }

    #[test]
    fn union_members_sent_as_null_are_unset() {
        let body = json!({
            "messages": [{"role":"user","content":[{"text":"hi","image":null}]}],
            "checks": {}
        });
        resp_json(&body);
        for bad in [
            json!({"text": null}),
            json!({"text": "hi", "image": {"format": "png"}}),
            json!({"image": {"format": "png"}}),
            json!({"text": 5}),
        ] {
            let body = json!({
                "messages": [{"role":"user","content":[bad]}],
                "checks": {}
            });
            assert_eq!(err_code(&body), "ValidationException", "{body}");
        }
    }

    #[test]
    fn contextual_entity_without_pattern_reports_nothing() {
        let body = json!({
            "messages": [{"role":"user","content":[{"text":"my name is Jane"}]}],
            "checks": {"sensitiveInformation": {"entities": [{"type":"NAME"}]}}
        });
        let r = resp_json(&body);
        assert!(r["results"]["sensitiveInformation"]["results"]
            .as_array()
            .unwrap()
            .is_empty());
    }

    #[test]
    fn model_constraints_are_enforced() {
        let ok_msgs = json!([{"role":"user","content":[{"text":"hi"}]}]);
        let cases = [
            // messages / checks are required
            json!({"checks": {}}),
            json!({"messages": ok_msgs}),
            // messages @length min 1
            json!({"messages": [], "checks": {}}),
            // role required + enum
            json!({"messages": [{"content":[{"text":"hi"}]}], "checks": {}}),
            json!({"messages": [{"role":"USER","content":[{"text":"hi"}]}], "checks": {}}),
            // content required, @length min 1, text @length min 1
            json!({"messages": [{"role":"user"}], "checks": {}}),
            json!({"messages": [{"role":"user","content":[]}], "checks": {}}),
            json!({"messages": [{"role":"user","content":[{"text":""}]}], "checks": {}}),
            json!({"messages": [{"role":"user","content":[{}]}], "checks": {}}),
            // category lists: required, @length 1..max, enum members
            json!({"messages": ok_msgs, "checks": {"contentFilter": {}}}),
            json!({"messages": ok_msgs, "checks": {"contentFilter": {"categories": []}}}),
            json!({"messages": ok_msgs, "checks": {"contentFilter": {"categories": [
                {"category":"VIOLENCE"},{"category":"HATE"},{"category":"SEXUAL"},
                {"category":"MISCONDUCT"},{"category":"INSULTS"},{"category":"HATE"}
            ]}}}),
            json!({"messages": ok_msgs, "checks": {"contentFilter": {"categories": [{"category":"GORE"}]}}}),
            json!({"messages": ok_msgs, "checks": {"promptAttack": {"categories": [{}]}}}),
            json!({"messages": ok_msgs, "checks": {"sensitiveInformation": {"entities": [{"type":"SSN"}]}}}),
        ];
        for body in cases {
            assert_eq!(err_code(&body), "ValidationException", "{body}");
        }
    }

    #[test]
    fn empty_checks_evaluate_nothing() {
        let body = json!({
            "messages": [{"role":"user","content":[{"text":"hi"}]}],
            "checks": {}
        });
        let r = resp_json(&body);
        assert_eq!(r["results"], json!({}));
        assert_eq!(r["usage"], json!({}));
    }
}
