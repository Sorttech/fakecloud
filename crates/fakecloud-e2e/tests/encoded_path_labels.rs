//! Path labels are percent-decoded exactly once, in core dispatch (#2605).
//!
//! The AWS SDKs percent-encode every `@httpLabel` value, so an ARN in
//! `/tags/{resourceArn}` travels as `arn%3Aaws%3A...%2F...`. These tests drive
//! TagResource + the matching tag read through several REST services with an
//! encoded ARN label and check the tags land on (and are read back from) the
//! real ARN. They cover services that used to hand-roll their own decoder
//! (Lambda, Batch, Scheduler, Pipes, Bedrock Agents, API Gateway v1/v2, DSQL),
//! a service that previously did not decode its labels at all (CloudFront's
//! `distributionsByWebACLId/{WebACLId}`), and the decode-once contract itself:
//! `%2525` is the literal `%25`, never `%`, and `+` stays `+` in a path.

use std::collections::HashMap;
use std::io::Write;

use aws_sdk_lambda::primitives::Blob;
use fakecloud_testkit::TestServer;
use serde_json::{json, Value};

fn python_zip() -> Vec<u8> {
    let cursor = std::io::Cursor::new(Vec::new());
    let mut writer = zip::ZipWriter::new(cursor);
    writer
        .start_file("index.py", zip::write::SimpleFileOptions::default())
        .unwrap();
    writer
        .write_all(b"def handler(event, context):\n    return {}\n")
        .unwrap();
    writer.finish().unwrap().into_inner()
}

fn one_tag() -> HashMap<String, String> {
    HashMap::from([("team".to_string(), "core".to_string())])
}

/// Percent-encode an ARN the way the SDKs encode a non-greedy label.
fn encode_label(value: &str) -> String {
    value
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// A SigV4-shaped Authorization header naming `service`, which is all
/// dispatch needs to route an unsigned raw request.
fn auth_for(service: &str) -> String {
    format!(
        "AWS4-HMAC-SHA256 Credential=test/20260101/us-east-1/{service}/aws4_request, \
         SignedHeaders=host, Signature=0"
    )
}

#[tokio::test]
async fn lambda_tags_with_encoded_function_arn() {
    let server = TestServer::start().await;
    let lambda = server.lambda_client().await;
    let arn = lambda
        .create_function()
        .function_name("encoded-label-fn")
        .runtime(aws_sdk_lambda::types::Runtime::Python312)
        .role("arn:aws:iam::123456789012:role/test-role")
        .handler("index.handler")
        .code(
            aws_sdk_lambda::types::FunctionCode::builder()
                .zip_file(Blob::new(python_zip()))
                .build(),
        )
        .send()
        .await
        .unwrap()
        .function_arn()
        .unwrap()
        .to_string();

    lambda
        .tag_resource()
        .resource(&arn)
        .set_tags(Some(one_tag()))
        .send()
        .await
        .unwrap();
    let tags = lambda.list_tags().resource(&arn).send().await.unwrap();
    assert_eq!(
        tags.tags().unwrap().get("team").map(String::as_str),
        Some("core")
    );

    // The ARN label also resolves the function itself.
    let got = lambda
        .get_function()
        .function_name(&arn)
        .send()
        .await
        .unwrap();
    assert_eq!(
        got.configuration().unwrap().function_name(),
        Some("encoded-label-fn")
    );
}

#[tokio::test]
async fn batch_tags_with_encoded_arn() {
    let server = TestServer::start().await;
    let batch = aws_sdk_batch::Client::new(&server.aws_config().await);
    let arn = "arn:aws:batch:us-east-1:123456789012:job-queue/encoded-q";
    batch
        .tag_resource()
        .resource_arn(arn)
        .set_tags(Some(one_tag()))
        .send()
        .await
        .unwrap();
    let out = batch
        .list_tags_for_resource()
        .resource_arn(arn)
        .send()
        .await
        .unwrap();
    assert_eq!(
        out.tags().unwrap().get("team").map(String::as_str),
        Some("core")
    );
}

#[tokio::test]
async fn scheduler_tags_with_encoded_group_arn() {
    let server = TestServer::start().await;
    let scheduler = server.scheduler_client().await;
    let arn = scheduler
        .create_schedule_group()
        .name("encoded-group")
        .send()
        .await
        .unwrap()
        .schedule_group_arn()
        .to_string();
    scheduler
        .tag_resource()
        .resource_arn(&arn)
        .tags(
            aws_sdk_scheduler::types::Tag::builder()
                .key("team")
                .value("core")
                .build()
                .unwrap(),
        )
        .send()
        .await
        .unwrap();
    let out = scheduler
        .list_tags_for_resource()
        .resource_arn(&arn)
        .send()
        .await
        .unwrap();
    let tags: Vec<(&str, &str)> = out.tags().iter().map(|t| (t.key(), t.value())).collect();
    assert_eq!(tags, vec![("team", "core")]);
}

#[tokio::test]
async fn pipes_tags_with_encoded_arn() {
    let server = TestServer::start().await;
    let pipes = server.pipes_client().await;
    let arn = "arn:aws:pipes:us-east-1:123456789012:pipe/encoded-pipe";
    pipes
        .tag_resource()
        .resource_arn(arn)
        .set_tags(Some(one_tag()))
        .send()
        .await
        .unwrap();
    let out = pipes
        .list_tags_for_resource()
        .resource_arn(arn)
        .send()
        .await
        .unwrap();
    assert_eq!(
        out.tags().unwrap().get("team").map(String::as_str),
        Some("core")
    );
}

#[tokio::test]
async fn bedrock_agent_tags_with_encoded_arn() {
    let server = TestServer::start().await;
    let agent = server.bedrock_agent_client().await;
    let arn = "arn:aws:bedrock:us-east-1:123456789012:agent/ENCODEDAG1";
    agent
        .tag_resource()
        .resource_arn(arn)
        .set_tags(Some(one_tag()))
        .send()
        .await
        .unwrap();
    let out = agent
        .list_tags_for_resource()
        .resource_arn(arn)
        .send()
        .await
        .unwrap();
    assert_eq!(
        out.tags().unwrap().get("team").map(String::as_str),
        Some("core")
    );
}

#[tokio::test]
async fn apigatewayv2_tags_with_encoded_api_arn() {
    let server = TestServer::start().await;
    let client = server.apigatewayv2_client().await;
    let api_id = client
        .create_api()
        .name("encoded-api")
        .protocol_type(aws_sdk_apigatewayv2::types::ProtocolType::Http)
        .send()
        .await
        .unwrap()
        .api_id()
        .unwrap()
        .to_string();
    let arn = format!("arn:aws:apigateway:us-east-1::/apis/{api_id}");
    client
        .tag_resource()
        .resource_arn(&arn)
        .set_tags(Some(one_tag()))
        .send()
        .await
        .unwrap();
    let out = client.get_tags().resource_arn(&arn).send().await.unwrap();
    assert_eq!(
        out.tags().unwrap().get("team").map(String::as_str),
        Some("core")
    );
}

#[tokio::test]
async fn apigateway_tag_labels_are_decoded_exactly_once() {
    let server = TestServer::start().await;
    let client = server.apigateway_client().await;
    // A stage ARN lands in the generic ARN-keyed tag store, so the stored key
    // is exactly what the handler received. The SDK sends the literal `%25`
    // below as `%2525`; decoding twice would store it under `100%`.
    let literal = "arn:aws:apigateway:us-east-1::/restapis/abc123/stages/100%25";
    client
        .tag_resource()
        .resource_arn(literal)
        .set_tags(Some(one_tag()))
        .send()
        .await
        .unwrap();
    let hit = client
        .get_tags()
        .resource_arn(literal)
        .send()
        .await
        .unwrap();
    assert_eq!(
        hit.tags().unwrap().get("team").map(String::as_str),
        Some("core")
    );
    let other = client
        .get_tags()
        .resource_arn("arn:aws:apigateway:us-east-1::/restapis/abc123/stages/100%")
        .send()
        .await
        .unwrap();
    assert!(
        other.tags().map(|t| t.is_empty()).unwrap_or(true),
        "`%2525` must decode once to `%25`, not collide with `%`: {:?}",
        other.tags()
    );
}

#[tokio::test]
async fn dsql_tags_with_encoded_cluster_arn() {
    let server = TestServer::start().await;
    let http = reqwest::Client::new();
    let base = server.endpoint();
    let created: Value = http
        .post(format!("{base}/cluster"))
        .header("Authorization", auth_for("dsql"))
        .header("content-type", "application/json")
        .body("{}")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let arn = created["arn"].as_str().expect("cluster arn").to_string();
    let path = format!("{base}/tags/{}", encode_label(&arn));
    assert!(path.contains("%3A") && path.contains("%2F"));

    let tag = http
        .post(&path)
        .header("Authorization", auth_for("dsql"))
        .header("content-type", "application/json")
        .body(json!({"tags": {"team": "core"}}).to_string())
        .send()
        .await
        .unwrap();
    assert!(tag.status().is_success(), "TagResource: {}", tag.status());
    let listed: Value = http
        .get(&path)
        .header("Authorization", auth_for("dsql"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(listed["tags"]["team"], "core");
}

#[tokio::test]
async fn cloudfront_web_acl_label_is_decoded() {
    #![allow(deprecated)]
    use aws_sdk_cloudfront::types::{
        CookiePreference, DefaultCacheBehavior, DistributionConfig, ForwardedValues, Headers,
        ItemSelection, Origin, Origins, ViewerProtocolPolicy,
    };
    let server = TestServer::start().await;
    let cf = server.cloudfront_client().await;
    // A WAFv2 web ACL is referenced by its full ARN, which carries both `:`
    // and `/` -- the SDK percent-encodes them in the `{WebACLId}` label.
    let web_acl =
        "arn:aws:wafv2:us-east-1:123456789012:global/webacl/encoded-acl/a1b2c3d4-0000-0000-0000-000000000000";
    let config = DistributionConfig::builder()
        .caller_reference("encoded-label")
        .comment("e2e")
        .enabled(true)
        .web_acl_id(web_acl)
        .origins(
            Origins::builder()
                .quantity(1)
                .items(
                    Origin::builder()
                        .id("primary")
                        .domain_name("example.com")
                        .build()
                        .unwrap(),
                )
                .build()
                .unwrap(),
        )
        .default_cache_behavior(
            DefaultCacheBehavior::builder()
                .target_origin_id("primary")
                .viewer_protocol_policy(ViewerProtocolPolicy::AllowAll)
                .forwarded_values(
                    ForwardedValues::builder()
                        .query_string(false)
                        .cookies(
                            CookiePreference::builder()
                                .forward(ItemSelection::None)
                                .build()
                                .unwrap(),
                        )
                        .headers(Headers::builder().quantity(0).build().unwrap())
                        .build()
                        .unwrap(),
                )
                .min_ttl(0)
                .build()
                .unwrap(),
        )
        .build()
        .unwrap();
    let dist_id = cf
        .create_distribution()
        .distribution_config(config)
        .send()
        .await
        .unwrap()
        .distribution()
        .unwrap()
        .id()
        .to_string();

    let out = cf
        .list_distributions_by_web_acl_id()
        .web_acl_id(web_acl)
        .send()
        .await
        .unwrap();
    let ids: Vec<&str> = out
        .distribution_list()
        .map(|l| l.items().iter().map(|d| d.id()).collect())
        .unwrap_or_default();
    assert_eq!(ids, vec![dist_id.as_str()]);
}

#[tokio::test]
async fn plus_in_a_path_label_is_a_literal_plus() {
    let server = TestServer::start().await;
    let ses = aws_sdk_sesv2::Client::new(&server.aws_config().await);
    ses.create_email_identity()
        .email_identity("a+b@example.com")
        .send()
        .await
        .unwrap();
    // The SDK sends `a%2Bb%40example.com`; a hand-written client may send the
    // `+` unescaped. In a path both mean `+` (only form data maps `+` to a
    // space), so both resolve the same identity.
    let via_sdk = ses
        .get_email_identity()
        .email_identity("a+b@example.com")
        .send()
        .await
        .unwrap();
    assert!(via_sdk.identity_type().is_some());
    let raw = reqwest::Client::new()
        .get(format!(
            "{}/v2/email/identities/a+b@example.com",
            server.endpoint()
        ))
        .header("Authorization", auth_for("ses"))
        .send()
        .await
        .unwrap();
    assert!(
        raw.status().is_success(),
        "raw `+` label must resolve `a+b@example.com`, got {}",
        raw.status()
    );
}
