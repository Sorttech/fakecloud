//! CloudFront E2E tests against the AWS Rust SDK.
//!
//! `forwarded_values` / `min_ttl` are deprecated on the SDK builder in
//! favour of policy IDs, but keeping the legacy fields exercised in our
//! roundtrip tests makes sure CloudFront responses remain backwards
//! compatible for older SDK clients.
#![allow(deprecated)]

mod helpers;

use aws_sdk_cloudfront::types::{
    CacheBehavior, CacheBehaviors, CachedMethods, CookiePreference, CustomErrorResponse,
    CustomErrorResponses, DefaultCacheBehavior, DistributionConfig, ForwardedValues,
    GeoRestriction, GeoRestrictionType, Headers, HttpVersion, ItemSelection, Method,
    MinimumProtocolVersion, Origin, Origins, Paths, PriceClass, Restrictions, ViewerCertificate,
    ViewerProtocolPolicy,
};
use helpers::TestServer;

fn minimal_config(caller_ref: &str) -> DistributionConfig {
    DistributionConfig::builder()
        .caller_reference(caller_ref)
        .comment("e2e")
        .enabled(true)
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
        .unwrap()
}

#[tokio::test]
async fn cloudfront_create_then_get_distribution() {
    let server = TestServer::start().await;
    let cf = server.cloudfront_client().await;
    let config = minimal_config("e2e-create");
    let create = cf
        .create_distribution()
        .distribution_config(config)
        .send()
        .await
        .expect("create_distribution");
    let dist = create.distribution().expect("distribution returned");
    let id = dist.id().to_string();
    assert!(!id.is_empty());
    assert!(dist.arn().contains(&id));
    assert!(dist.domain_name().ends_with(".cloudfront.net"));
    let etag = create.e_tag().expect("etag header");
    assert!(!etag.is_empty());

    let got = cf
        .get_distribution()
        .id(&id)
        .send()
        .await
        .expect("get_distribution");
    assert_eq!(got.distribution().unwrap().id(), id);
}

#[tokio::test]
async fn cloudfront_list_distributions() {
    let server = TestServer::start().await;
    let cf = server.cloudfront_client().await;
    cf.create_distribution()
        .distribution_config(minimal_config("e2e-list"))
        .send()
        .await
        .expect("create_distribution");

    let list = cf
        .list_distributions()
        .send()
        .await
        .expect("list_distributions");
    let lst = list.distribution_list().expect("list shape");
    assert!(lst.quantity() >= 1);
    let items = lst.items();
    assert!(!items.is_empty());
}

#[tokio::test]
async fn cloudfront_update_distribution_requires_if_match() {
    let server = TestServer::start().await;
    let cf = server.cloudfront_client().await;
    let create = cf
        .create_distribution()
        .distribution_config(minimal_config("e2e-update"))
        .send()
        .await
        .expect("create_distribution");
    let dist = create.distribution().unwrap();
    let id = dist.id().to_string();
    let etag = create.e_tag().unwrap().to_string();
    let mut new_config = minimal_config("e2e-update");
    new_config = DistributionConfig::builder()
        .caller_reference(new_config.caller_reference())
        .comment("e2e-updated")
        .enabled(true)
        .origins(new_config.origins().unwrap().clone())
        .default_cache_behavior(new_config.default_cache_behavior().unwrap().clone())
        .build()
        .unwrap();

    cf.update_distribution()
        .id(&id)
        .if_match(&etag)
        .distribution_config(new_config)
        .send()
        .await
        .expect("update_distribution");

    let got = cf.get_distribution().id(&id).send().await.unwrap();
    assert_eq!(
        got.distribution()
            .unwrap()
            .distribution_config()
            .unwrap()
            .comment(),
        "e2e-updated"
    );
}

/// CloudFront errors use the REST-XML `<ErrorResponse>` wrapper, so the SDK
/// parses them into the typed modeled error (#2632).
#[tokio::test]
async fn cloudfront_errors_are_typed_sdk_errors() {
    use aws_sdk_cloudfront::error::ProvideErrorMetadata;

    let server = TestServer::start().await;
    let cf = server.cloudfront_client().await;

    let err = cf
        .get_distribution()
        .id("EDOESNOTEXIST")
        .send()
        .await
        .unwrap_err()
        .into_service_error();
    assert!(err.is_no_such_distribution(), "{err:?}");
    assert_eq!(err.code(), Some("NoSuchDistribution"));
    assert!(
        err.message().is_some_and(|m| !m.is_empty()),
        "message must be parsed: {err:?}"
    );

    let create = cf
        .create_distribution()
        .distribution_config(minimal_config("e2e-typed-err"))
        .send()
        .await
        .expect("create_distribution");
    let id = create.distribution().unwrap().id().to_string();
    let err = cf
        .update_distribution()
        .id(&id)
        .if_match("ESTALEETAG")
        .distribution_config(minimal_config("e2e-typed-err"))
        .send()
        .await
        .unwrap_err()
        .into_service_error();
    assert!(err.is_precondition_failed(), "{err:?}");
}

#[tokio::test]
async fn cloudfront_create_invalidation() {
    let server = TestServer::start().await;
    let cf = server.cloudfront_client().await;
    let create = cf
        .create_distribution()
        .distribution_config(minimal_config("e2e-inv"))
        .send()
        .await
        .expect("create_distribution");
    let id = create.distribution().unwrap().id().to_string();
    let inv = cf
        .create_invalidation()
        .distribution_id(&id)
        .invalidation_batch(
            aws_sdk_cloudfront::types::InvalidationBatch::builder()
                .caller_reference("inv-1")
                .paths(
                    Paths::builder()
                        .quantity(1)
                        .items("/static/*")
                        .build()
                        .unwrap(),
                )
                .build()
                .unwrap(),
        )
        .send()
        .await
        .expect("create_invalidation");
    assert!(inv.invalidation().unwrap().id().starts_with('I'));

    let list = cf
        .list_invalidations()
        .distribution_id(&id)
        .send()
        .await
        .expect("list_invalidations");
    assert_eq!(list.invalidation_list().unwrap().quantity(), 1);
}

#[tokio::test]
async fn cloudfront_tags_roundtrip() {
    let server = TestServer::start().await;
    let cf = server.cloudfront_client().await;
    let create = cf
        .create_distribution()
        .distribution_config(minimal_config("e2e-tags"))
        .send()
        .await
        .expect("create_distribution");
    let arn = create.distribution().unwrap().arn().to_string();
    cf.tag_resource()
        .resource(&arn)
        .tags(
            aws_sdk_cloudfront::types::Tags::builder()
                .items(
                    aws_sdk_cloudfront::types::Tag::builder()
                        .key("env")
                        .value("prod")
                        .build()
                        .unwrap(),
                )
                .build(),
        )
        .send()
        .await
        .expect("tag_resource");

    let list = cf
        .list_tags_for_resource()
        .resource(&arn)
        .send()
        .await
        .expect("list_tags_for_resource");
    let tags = list.tags().unwrap().items();
    assert!(tags
        .iter()
        .any(|t| t.key() == "env" && t.value() == Some("prod")));

    cf.untag_resource()
        .resource(&arn)
        .tag_keys(
            aws_sdk_cloudfront::types::TagKeys::builder()
                .items("env")
                .build(),
        )
        .send()
        .await
        .expect("untag_resource");

    let after = cf
        .list_tags_for_resource()
        .resource(&arn)
        .send()
        .await
        .expect("list_tags_for_resource");
    let tags = after.tags().unwrap().items();
    assert!(!tags.iter().any(|t| t.key() == "env"));
}

#[tokio::test]
async fn cloudfront_complex_config_roundtrips() {
    let server = TestServer::start().await;
    let cf = server.cloudfront_client().await;
    let config = DistributionConfig::builder()
        .caller_reference("complex")
        .comment("complex")
        .enabled(true)
        .price_class(PriceClass::PriceClass100)
        .http_version(HttpVersion::Http2)
        .is_ipv6_enabled(true)
        .origins(
            Origins::builder()
                .quantity(1)
                .items(
                    Origin::builder()
                        .id("origin-1")
                        .domain_name("origin.example.com")
                        .build()
                        .unwrap(),
                )
                .build()
                .unwrap(),
        )
        .default_cache_behavior(
            DefaultCacheBehavior::builder()
                .target_origin_id("origin-1")
                .viewer_protocol_policy(ViewerProtocolPolicy::RedirectToHttps)
                .compress(true)
                .allowed_methods(
                    aws_sdk_cloudfront::types::AllowedMethods::builder()
                        .quantity(2)
                        .items(Method::Get)
                        .items(Method::Head)
                        .cached_methods(
                            CachedMethods::builder()
                                .quantity(2)
                                .items(Method::Get)
                                .items(Method::Head)
                                .build()
                                .unwrap(),
                        )
                        .build()
                        .unwrap(),
                )
                .min_ttl(0)
                .default_ttl(86400)
                .max_ttl(31536000)
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
                .build()
                .unwrap(),
        )
        .cache_behaviors(
            CacheBehaviors::builder()
                .quantity(1)
                .items(
                    CacheBehavior::builder()
                        .path_pattern("/api/*")
                        .target_origin_id("origin-1")
                        .viewer_protocol_policy(ViewerProtocolPolicy::HttpsOnly)
                        .min_ttl(0)
                        .forwarded_values(
                            ForwardedValues::builder()
                                .query_string(true)
                                .cookies(
                                    CookiePreference::builder()
                                        .forward(ItemSelection::All)
                                        .build()
                                        .unwrap(),
                                )
                                .headers(Headers::builder().quantity(0).build().unwrap())
                                .build()
                                .unwrap(),
                        )
                        .build()
                        .unwrap(),
                )
                .build()
                .unwrap(),
        )
        .custom_error_responses(
            CustomErrorResponses::builder()
                .quantity(1)
                .items(
                    CustomErrorResponse::builder()
                        .error_code(403)
                        .response_page_path("/403.html")
                        .response_code("403")
                        .error_caching_min_ttl(10)
                        .build()
                        .unwrap(),
                )
                .build()
                .unwrap(),
        )
        .restrictions(
            Restrictions::builder()
                .geo_restriction(
                    GeoRestriction::builder()
                        .restriction_type(GeoRestrictionType::Whitelist)
                        .quantity(1)
                        .items("US")
                        .build()
                        .unwrap(),
                )
                .build(),
        )
        .viewer_certificate(
            ViewerCertificate::builder()
                .cloud_front_default_certificate(true)
                .minimum_protocol_version(MinimumProtocolVersion::TlSv122021)
                .build(),
        )
        .build()
        .unwrap();

    let create = cf
        .create_distribution()
        .distribution_config(config)
        .send()
        .await
        .expect("create_distribution");
    let id = create.distribution().unwrap().id().to_string();

    let got = cf
        .get_distribution_config()
        .id(&id)
        .send()
        .await
        .expect("get_distribution_config");
    let cfg = got.distribution_config().unwrap();
    assert_eq!(cfg.cache_behaviors().unwrap().quantity(), 1);
    assert_eq!(cfg.custom_error_responses().unwrap().quantity(), 1);
    assert_eq!(
        cfg.restrictions()
            .unwrap()
            .geo_restriction()
            .unwrap()
            .restriction_type(),
        &GeoRestrictionType::Whitelist
    );
    assert_eq!(cfg.price_class(), Some(&PriceClass::PriceClass100));
    // Members AWS spells with an upper-case acronym (MinTTL, IsIPV6Enabled,
    // ErrorCachingMinTTL) must survive the parse and the render.
    assert_eq!(cfg.is_ipv6_enabled(), Some(true));
    let dcb = cfg.default_cache_behavior().unwrap();
    assert_eq!(dcb.min_ttl(), Some(0));
    assert_eq!(dcb.default_ttl(), Some(86400));
    assert_eq!(dcb.max_ttl(), Some(31536000));
    assert_eq!(
        cfg.custom_error_responses().unwrap().items()[0].error_caching_min_ttl(),
        Some(10)
    );
}
