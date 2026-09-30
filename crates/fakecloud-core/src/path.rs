//! URI path handling shared by every REST service.
//!
//! Smithy `@httpLabel` semantics: the client percent-encodes each label
//! value, so a `/` inside a non-greedy label travels as `%2F` and `:` in an
//! ARN usually travels as `%3A`. The server must therefore split the raw
//! path on `/` FIRST and percent-decode each segment afterwards; decoding
//! before splitting would turn an encoded `/` into a segment boundary. A
//! greedy label (`{Key+}`) is the decoded segments re-joined with `/`.
//!
//! Dispatch builds [`crate::service::AwsRequest::path_segments`] with
//! [`split_path_segments`], so handlers receive decoded labels and must not
//! decode them again (a label of `%2525` is the literal `%25`, not `%`).
//! Routers that need the undecoded wire path read `raw_path` instead.

/// Percent-decode one URI path segment per RFC 3986. `+` is a literal `+`
/// in a path (only `application/x-www-form-urlencoded` data maps it to a
/// space). A malformed escape (`%zz`, a trailing `%`) is kept verbatim, and
/// bytes that do not form valid UTF-8 are replaced lossily -- decoding never
/// fails and never panics.
pub fn percent_decode_segment(segment: &str) -> String {
    if !segment.contains('%') {
        return segment.to_string();
    }
    percent_encoding::percent_decode_str(segment)
        .decode_utf8_lossy()
        .into_owned()
}

/// Split a raw URI path into decoded segments: split on `/`, drop empty
/// segments (leading, trailing and doubled slashes), then percent-decode
/// each segment exactly once.
pub fn split_path_segments(raw_path: &str) -> Vec<String> {
    raw_path
        .split('/')
        .filter(|s| !s.is_empty())
        .map(percent_decode_segment)
        .collect()
}

/// Split a raw URI path into its undecoded segments, dropping empty ones.
/// This is the wire form of [`split_path_segments`] for handlers that must
/// forward or match the path exactly as the client sent it (API Gateway
/// execute-api data plane, for one).
pub fn split_raw_path_segments(raw_path: &str) -> Vec<String> {
    raw_path
        .split('/')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_encoded_colon_in_arn_label() {
        assert_eq!(
            split_path_segments(
                "/tags/arn%3Aaws%3Abatch%3Aus-east-1%3A123456789012%3Ajob-queue%2Fq"
            ),
            vec![
                "tags".to_string(),
                "arn:aws:batch:us-east-1:123456789012:job-queue/q".to_string()
            ]
        );
    }

    #[test]
    fn encoded_slash_stays_inside_its_non_greedy_label() {
        // Split happens before decoding, so `%2F` does not create a segment.
        assert_eq!(
            split_path_segments("/v1/pipes/a%2Fb/start"),
            vec!["v1", "pipes", "a/b", "start"]
        );
    }

    #[test]
    fn double_encoded_percent_decodes_exactly_once() {
        assert_eq!(split_path_segments("/x/100%2525"), vec!["x", "100%25"]);
        assert_eq!(percent_decode_segment("%2525"), "%25");
    }

    #[test]
    fn greedy_label_is_the_rejoined_decoded_segments() {
        let segs = split_path_segments("/bucket/dir%20one/sub/file%3Dx.txt");
        assert_eq!(segs[1..].join("/"), "dir one/sub/file=x.txt");
    }

    #[test]
    fn plus_is_literal_in_paths() {
        assert_eq!(split_path_segments("/k/a+b%2Bc"), vec!["k", "a+b+c"]);
    }

    #[test]
    fn empty_segments_are_dropped() {
        assert_eq!(split_path_segments("//a//b/"), vec!["a", "b"]);
        assert!(split_path_segments("/").is_empty());
    }

    #[test]
    fn malformed_escapes_and_multibyte_are_safe() {
        assert_eq!(percent_decode_segment("%zz%"), "%zz%");
        assert_eq!(percent_decode_segment("%€"), "%€");
        assert_eq!(percent_decode_segment("caf%C3%A9"), "café");
        assert_eq!(percent_decode_segment("%FF"), "\u{FFFD}");
    }

    #[test]
    fn raw_segments_are_not_decoded() {
        assert_eq!(
            split_raw_path_segments("/prod/a%2Fb//c%3A"),
            vec!["prod", "a%2Fb", "c%3A"]
        );
    }
}
