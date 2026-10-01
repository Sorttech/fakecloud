//! CloudFront account isolation (#2603).
//!
//! CloudFront is global (no region), but every resource belongs to the account
//! that created it: another account can neither see nor change it. Viewer
//! traffic carries no account, so the data plane still serves a distribution by
//! its domain whichever account owns it.

mod helpers;

use aws_credential_types::Credentials;
use aws_sdk_cloudfront::primitives::Blob;
use aws_sdk_cloudfront::types::{
    CustomOriginConfig, DefaultCacheBehavior, DistributionConfig, FunctionConfig, FunctionRuntime,
    InvalidationBatch, Origin, OriginAccessControlConfig, OriginAccessControlOriginTypes,
    OriginAccessControlSigningBehaviors, OriginAccessControlSigningProtocols, OriginProtocolPolicy,
    Origins, Paths, ViewerProtocolPolicy,
};
use aws_sdk_cloudfront::Client as CloudFrontClient;
use helpers::TestServer;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const ACCOUNT_A: &str = "123456789012";
const ACCOUNT_B: &str = "222222222222";

async fn client_in(server: &TestServer, account: &str, name: &str) -> CloudFrontClient {
    let (akid, secret) = server.create_admin(account, name).await;
    let cfg = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .endpoint_url(server.endpoint())
        .region(aws_config::Region::new("us-east-1"))
        .credentials_provider(Credentials::new(akid, secret, None, None, "cf-multi-acct"))
        .load()
        .await;
    CloudFrontClient::new(&cfg)
}

/// A one-shot-per-connection HTTP origin answering `ORIGIN <path>`.
async fn start_origin() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                let path = req
                    .lines()
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or("/")
                    .to_string();
                let body = format!("ORIGIN {path}");
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            });
        }
    });
    port
}

fn distribution_config(caller_reference: &str, origin: &str, enabled: bool) -> DistributionConfig {
    DistributionConfig::builder()
        .caller_reference(caller_reference)
        .comment("")
        .enabled(enabled)
        .origins(
            Origins::builder()
                .quantity(1)
                .items(
                    Origin::builder()
                        .id("o1")
                        .domain_name(origin)
                        .custom_origin_config(
                            CustomOriginConfig::builder()
                                .http_port(80)
                                .https_port(443)
                                .origin_protocol_policy(OriginProtocolPolicy::HttpOnly)
                                .build()
                                .unwrap(),
                        )
                        .build()
                        .unwrap(),
                )
                .build()
                .unwrap(),
        )
        .default_cache_behavior(
            DefaultCacheBehavior::builder()
                .target_origin_id("o1")
                .viewer_protocol_policy(ViewerProtocolPolicy::AllowAll)
                .cache_policy_id("4135ea2d-6df8-44a3-9df3-4b5a84be39ad")
                .build()
                .unwrap(),
        )
        .build()
        .unwrap()
}

#[tokio::test]
async fn another_account_cannot_see_or_change_a_distribution() {
    let server = TestServer::start_with_env(&[("FAKECLOUD_IAM", "soft")]).await;
    let cf_a = client_in(&server, ACCOUNT_A, "admin-a").await;
    let cf_b = client_in(&server, ACCOUNT_B, "admin-b").await;
    let origin = format!("127.0.0.1:{}", start_origin().await);

    let created = cf_a
        .create_distribution()
        .distribution_config(distribution_config("ref-a", &origin, true))
        .send()
        .await
        .expect("A creates a distribution");
    let dist = created.distribution().unwrap();
    let id = dist.id().to_string();
    let etag = created.e_tag().unwrap().to_string();
    assert_eq!(
        dist.arn(),
        format!("arn:aws:cloudfront::{ACCOUNT_A}:distribution/{id}")
    );

    // B: no read, no listing, no update, no delete, no invalidation.
    let err = cf_b.get_distribution().id(&id).send().await.unwrap_err();
    assert!(err.into_service_error().is_no_such_distribution());
    let listed = cf_b.list_distributions().send().await.unwrap();
    let b_ids: Vec<&str> = listed
        .distribution_list()
        .map(|l| l.items().iter().map(|d| d.id()).collect())
        .unwrap_or_default();
    assert!(!b_ids.contains(&id.as_str()), "{b_ids:?}");
    let err = cf_b
        .update_distribution()
        .id(&id)
        .if_match(&etag)
        .distribution_config(distribution_config("ref-a", &origin, false))
        .send()
        .await
        .unwrap_err();
    assert!(err.into_service_error().is_no_such_distribution());
    let err = cf_b
        .delete_distribution()
        .id(&id)
        .if_match(&etag)
        .send()
        .await
        .unwrap_err();
    assert!(err.into_service_error().is_no_such_distribution());
    let err = cf_b
        .create_invalidation()
        .distribution_id(&id)
        .invalidation_batch(
            InvalidationBatch::builder()
                .caller_reference("inv-b")
                .paths(Paths::builder().quantity(1).items("/*").build().unwrap())
                .build()
                .unwrap(),
        )
        .send()
        .await
        .unwrap_err();
    assert!(err.into_service_error().is_no_such_distribution());

    // B reusing A's CallerReference is its own, independent distribution.
    let b_dist = cf_b
        .create_distribution()
        .distribution_config(distribution_config("ref-a", &origin, true))
        .send()
        .await
        .expect("CallerReference is per account");
    let b_dist = b_dist.distribution().unwrap();
    assert_ne!(b_dist.id(), id);
    assert!(b_dist.arn().contains(ACCOUNT_B), "{}", b_dist.arn());

    // A still owns its distribution, unchanged, and sees only its own.
    let got = cf_a.get_distribution().id(&id).send().await.unwrap();
    assert_eq!(got.e_tag(), Some(etag.as_str()));
    assert!(got
        .distribution()
        .unwrap()
        .distribution_config()
        .unwrap()
        .enabled());
    let listed = cf_a.list_distributions().send().await.unwrap();
    let a_ids: Vec<&str> = listed
        .distribution_list()
        .map(|l| l.items().iter().map(|d| d.id()).collect())
        .unwrap_or_default();
    assert_eq!(a_ids, [id.as_str()]);

    // Viewer traffic carries no account: A's distribution is served by its
    // domain regardless.
    let resp = reqwest::Client::new()
        .get(format!("{}/hello", server.endpoint()))
        .header(reqwest::header::HOST, dist.domain_name())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "ORIGIN /hello");

    // Resetting B's CloudFront state leaves A's alone.
    let reset = reqwest::Client::new()
        .post(format!(
            "{}/_fakecloud/reset/cloudfront/{ACCOUNT_B}",
            server.endpoint()
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(reset.status(), 200);
    cf_a.get_distribution()
        .id(&id)
        .send()
        .await
        .expect("A's survives");
    let err = cf_b
        .get_distribution()
        .id(b_dist.id())
        .send()
        .await
        .unwrap_err();
    assert!(err.into_service_error().is_no_such_distribution());
}

#[tokio::test]
async fn another_account_cannot_see_or_change_policies_or_functions() {
    let server = TestServer::start_with_env(&[("FAKECLOUD_IAM", "soft")]).await;
    let cf_a = client_in(&server, ACCOUNT_A, "admin-a").await;
    let cf_b = client_in(&server, ACCOUNT_B, "admin-b").await;

    let oac = cf_a
        .create_origin_access_control()
        .origin_access_control_config(
            OriginAccessControlConfig::builder()
                .name("oac-a")
                .origin_access_control_origin_type(OriginAccessControlOriginTypes::S3)
                .signing_behavior(OriginAccessControlSigningBehaviors::Always)
                .signing_protocol(OriginAccessControlSigningProtocols::Sigv4)
                .build()
                .unwrap(),
        )
        .send()
        .await
        .unwrap();
    let oac_id = oac.origin_access_control().unwrap().id().to_string();
    let oac_etag = oac.e_tag().unwrap().to_string();

    let function = cf_a
        .create_function()
        .name("fn-a")
        .function_config(
            FunctionConfig::builder()
                .comment("a")
                .runtime(FunctionRuntime::CloudfrontJs20)
                .build()
                .unwrap(),
        )
        .function_code(Blob::new(
            b"function handler(event) { return event.request; }",
        ))
        .send()
        .await
        .unwrap();
    let fn_etag = function.e_tag().unwrap().to_string();
    let fn_arn = function
        .function_summary()
        .and_then(|s| s.function_metadata())
        .map(|m| m.function_arn().to_string())
        .unwrap();
    assert!(fn_arn.contains(ACCOUNT_A), "{fn_arn}");

    // B cannot read, list, or delete A's OAC.
    assert!(cf_b
        .get_origin_access_control()
        .id(&oac_id)
        .send()
        .await
        .unwrap_err()
        .into_service_error()
        .is_no_such_origin_access_control());
    let b_oacs = cf_b.list_origin_access_controls().send().await.unwrap();
    assert_eq!(
        b_oacs
            .origin_access_control_list()
            .map(|l| l.quantity())
            .unwrap_or_default(),
        0
    );
    assert!(cf_b
        .delete_origin_access_control()
        .id(&oac_id)
        .if_match(&oac_etag)
        .send()
        .await
        .unwrap_err()
        .into_service_error()
        .is_no_such_origin_access_control());

    // B cannot describe, list, or delete A's function; the name is free in B.
    let err = cf_b
        .describe_function()
        .name("fn-a")
        .send()
        .await
        .unwrap_err();
    assert!(err.into_service_error().is_no_such_function_exists());
    let b_fns = cf_b.list_functions().send().await.unwrap();
    assert_eq!(
        b_fns
            .function_list()
            .map(|l| l.quantity())
            .unwrap_or_default(),
        0
    );
    assert!(cf_b
        .delete_function()
        .name("fn-a")
        .if_match(&fn_etag)
        .send()
        .await
        .unwrap_err()
        .into_service_error()
        .is_no_such_function_exists());
    cf_b.create_function()
        .name("fn-a")
        .function_config(
            FunctionConfig::builder()
                .comment("b")
                .runtime(FunctionRuntime::CloudfrontJs20)
                .build()
                .unwrap(),
        )
        .function_code(Blob::new(
            b"function handler(event) { return event.request; }",
        ))
        .send()
        .await
        .expect("function names are per account");

    // A's resources are intact; AWS-managed policies stay visible to both.
    cf_a.get_origin_access_control()
        .id(&oac_id)
        .send()
        .await
        .expect("A's OAC survives");
    let described = cf_a.describe_function().name("fn-a").send().await.unwrap();
    assert_eq!(
        described
            .function_summary()
            .and_then(|s| s.function_config())
            .map(|c| c.comment()),
        Some("a")
    );
    for cf in [&cf_a, &cf_b] {
        cf.get_cache_policy()
            .id("658327ea-f89d-4fab-a63d-7e88639e58f6")
            .send()
            .await
            .expect("managed cache policy visible");
    }
}
