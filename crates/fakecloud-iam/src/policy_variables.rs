//! IAM policy variables: `${aws:username}`-style placeholders in `Resource` /
//! `NotResource` ARNs and in string and ARN condition values.
//!
//! Semantics follow the IAM User Guide ("IAM policy elements: Variables and
//! tags"):
//!
//! - Only a policy whose `Version` is `2012-10-17` expands variables; in any
//!   other policy `${...}` is literal text.
//! - `${key}` is replaced by the request's value for the (case-insensitive)
//!   condition key. A key with no value -- absent, or multivalued, which
//!   cannot be used as a variable -- makes the string null: it matches no
//!   resource, positive operators (`StringEquals`, `StringLike`, `ArnLike`,
//!   ...) never match it, and inverted ones (`StringNotEquals`, ...) do.
//! - `${key, 'default'}` falls back to `default` when the key has no value.
//! - `${*}`, `${?}` and `${$}` stand for a literal `*`, `?` and `$`.
//! - A substituted value is literal: a `*` in a principal's tag is not a
//!   wildcard, so a tag value cannot widen what a policy grants.

use fakecloud_core::auth::ConditionContext;

/// One element of a policy string after expansion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Piece {
    Literal(char),
    /// `*` written in the policy: any run of characters.
    AnyRun,
    /// `?` written in the policy: any one character.
    AnyOne,
}

/// Whether `text` contains a variable reference at all. Strings without one
/// keep their existing matching path.
pub(crate) fn has_variables(text: &str) -> bool {
    text.contains("${")
}

/// Expand `text`, or `None` when a variable in it has no value (and no
/// default).
pub(crate) fn expand(text: &str, ctx: &ConditionContext) -> Option<Vec<Piece>> {
    let mut out = Vec::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("${") {
        push_policy_text(&mut out, &rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            // Unterminated: the rest is plain text.
            push_policy_text(&mut out, &rest[start..]);
            return Some(out);
        };
        let inner = &after[..end];
        match inner.trim() {
            "*" => out.push(Piece::Literal('*')),
            "?" => out.push(Piece::Literal('?')),
            "$" => out.push(Piece::Literal('$')),
            reference => {
                let (key, default) = split_default(reference);
                let value = match ctx.lookup(key) {
                    Some(values) if values.len() == 1 => values.into_iter().next(),
                    _ => default.map(str::to_string),
                }?;
                out.extend(value.chars().map(Piece::Literal));
            }
        }
        rest = &after[end + 1..];
    }
    push_policy_text(&mut out, rest);
    Some(out)
}

/// `aws:PrincipalTag/team, 'company-wide'` -> (`aws:PrincipalTag/team`,
/// `Some("company-wide")`).
fn split_default(reference: &str) -> (&str, Option<&str>) {
    if let Some((key, default)) = reference.split_once(',') {
        let default = default.trim();
        if let Some(quoted) = default
            .strip_prefix('\'')
            .and_then(|d| d.strip_suffix('\''))
        {
            return (key.trim(), Some(quoted));
        }
    }
    (reference, None)
}

fn push_policy_text(out: &mut Vec<Piece>, text: &str) {
    out.extend(text.chars().map(|c| match c {
        '*' => Piece::AnyRun,
        '?' => Piece::AnyOne,
        c => Piece::Literal(c),
    }));
}

/// The expanded string as plain text, for exact comparisons: a wildcard
/// written in the policy is just its character there.
pub(crate) fn to_text(pieces: &[Piece]) -> String {
    pieces
        .iter()
        .map(|p| match p {
            Piece::Literal(c) => *c,
            Piece::AnyRun => '*',
            Piece::AnyOne => '?',
        })
        .collect()
}

/// Glob-match `value` against expanded pieces, optionally ignoring ASCII
/// case.
pub(crate) fn glob(pieces: &[Piece], value: &str, ignore_case: bool) -> bool {
    let v: Vec<char> = value.chars().collect();
    let eq = |a: char, b: char| {
        if ignore_case {
            a.eq_ignore_ascii_case(&b)
        } else {
            a == b
        }
    };
    let (mut pi, mut vi) = (0usize, 0usize);
    let mut star: Option<usize> = None;
    let mut star_v = 0usize;
    while vi < v.len() {
        match pieces.get(pi) {
            Some(Piece::AnyOne) => {
                pi += 1;
                vi += 1;
            }
            Some(Piece::Literal(c)) if eq(*c, v[vi]) => {
                pi += 1;
                vi += 1;
            }
            Some(Piece::AnyRun) => {
                star = Some(pi);
                star_v = vi;
                pi += 1;
            }
            _ => match star {
                Some(s) => {
                    pi = s + 1;
                    star_v += 1;
                    vi = star_v;
                }
                None => return false,
            },
        }
    }
    while matches!(pieces.get(pi), Some(Piece::AnyRun)) {
        pi += 1;
    }
    pi == pieces.len()
}

/// Match a `Resource` / `NotResource` pattern that carries variables.
/// Variables are expanded only in the resource part of the ARN, after the
/// fifth colon; `None` when a variable there has no value, which matches no
/// resource.
pub(crate) fn resource_pattern(pattern: &str, ctx: &ConditionContext) -> Option<Vec<Piece>> {
    let split = pattern
        .match_indices(':')
        .nth(4)
        .map(|(i, _)| i + 1)
        .unwrap_or(0);
    let mut pieces = Vec::new();
    push_policy_text(&mut pieces, &pattern[..split]);
    pieces.extend(expand(&pattern[split..], ctx)?);
    Some(pieces)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn ctx() -> ConditionContext {
        ConditionContext {
            aws_username: Some("alice".to_string()),
            principal_tags: Some(HashMap::from([
                ("team".to_string(), "blue".to_string()),
                ("weird".to_string(), "a*b".to_string()),
            ])),
            ..Default::default()
        }
    }

    #[test]
    fn expands_keys_defaults_and_special_characters() {
        let c = ctx();
        assert_eq!(
            to_text(&expand("home/${aws:username}/*", &c).unwrap()),
            "home/alice/*"
        );
        assert_eq!(to_text(&expand("${AWS:UserName}", &c).unwrap()), "alice");
        assert_eq!(
            to_text(&expand("${aws:PrincipalTag/missing, 'company-wide'}", &c).unwrap()),
            "company-wide"
        );
        assert_eq!(expand("x-${aws:PrincipalTag/missing}", &c), None);
        assert_eq!(to_text(&expand("a${*}b${?}c${$}", &c).unwrap()), "a*b?c$");
        assert_eq!(
            to_text(&expand("open ${aws:username", &c).unwrap()),
            "open ${aws:username"
        );
    }

    #[test]
    fn substituted_values_and_special_characters_are_literal() {
        let c = ctx();
        let pieces = expand("${aws:PrincipalTag/weird}", &c).unwrap();
        assert!(glob(&pieces, "a*b", false));
        assert!(!glob(&pieces, "aXXb", false), "a tag's * is not a wildcard");
        let pieces = expand("file${*}", &c).unwrap();
        assert!(glob(&pieces, "file*", false));
        assert!(!glob(&pieces, "file1", false));
        let pieces = expand("home/${aws:username}/*", &c).unwrap();
        assert!(glob(&pieces, "home/alice/doc.txt", false));
        assert!(!glob(&pieces, "home/bob/doc.txt", false));
    }

    #[test]
    fn resource_patterns_expand_only_the_resource_part() {
        let c = ctx();
        let pieces = resource_pattern("arn:aws:s3:::bucket/${aws:username}/*", &c).unwrap();
        assert!(glob(&pieces, "arn:aws:s3:::bucket/alice/x", false));
        assert!(!glob(&pieces, "arn:aws:s3:::bucket/bob/x", false));
        assert_eq!(
            resource_pattern("arn:aws:s3:::bucket/${aws:PrincipalTag/none}", &c),
            None
        );
    }
}
