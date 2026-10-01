//! CloudFront Batch 2 E2E: Origin Access Controls + Cache, Origin
//! Request, Response Headers, and Continuous Deployment policies.
#![allow(deprecated)]

mod helpers;

use aws_sdk_cloudfront::types::{
    CachePolicyConfig, CachePolicyCookieBehavior, CachePolicyCookiesConfig,
    CachePolicyHeaderBehavior, CachePolicyHeadersConfig, CachePolicyQueryStringBehavior,
    CachePolicyQueryStringsConfig, ContinuousDeploymentPolicyConfig,
    ContinuousDeploymentPolicyType, ContinuousDeploymentSingleHeaderConfig,
    OriginAccessControlConfig, OriginAccessControlOriginTypes, OriginAccessControlSigningBehaviors,
    OriginAccessControlSigningProtocols, OriginRequestPolicyConfig,
    OriginRequestPolicyCookieBehavior, OriginRequestPolicyCookiesConfig,
    OriginRequestPolicyHeaderBehavior, OriginRequestPolicyHeadersConfig,
    OriginRequestPolicyQueryStringBehavior, OriginRequestPolicyQueryStringsConfig,
    ParametersInCacheKeyAndForwardedToOrigin, ResponseHeadersPolicyAccessControlAllowHeaders,
    ResponseHeadersPolicyAccessControlAllowMethods,
    ResponseHeadersPolicyAccessControlAllowMethodsValues,
    ResponseHeadersPolicyAccessControlAllowOrigins, ResponseHeadersPolicyConfig,
    ResponseHeadersPolicyCorsConfig, StagingDistributionDnsNames, TrafficConfig,
};
use helpers::TestServer;

#[tokio::test]
async fn origin_access_control_lifecycle() {
    let server = TestServer::start().await;
    let cf = server.cloudfront_client().await;

    let cfg = OriginAccessControlConfig::builder()
        .name("oac-e2e")
        .description("test oac")
        .origin_access_control_origin_type(OriginAccessControlOriginTypes::S3)
        .signing_behavior(OriginAccessControlSigningBehaviors::Always)
        .signing_protocol(OriginAccessControlSigningProtocols::Sigv4)
        .build()
        .unwrap();

    let create = cf
        .create_origin_access_control()
        .origin_access_control_config(cfg)
        .send()
        .await
        .expect("create_oac");
    let oac = create.origin_access_control().expect("oac");
    let id = oac.id().to_string();
    let etag = create.e_tag().expect("etag").to_string();

    let got = cf
        .get_origin_access_control()
        .id(&id)
        .send()
        .await
        .expect("get_oac");
    assert_eq!(got.origin_access_control().unwrap().id(), id);

    let new_cfg = OriginAccessControlConfig::builder()
        .name("oac-e2e-renamed")
        .description("updated")
        .origin_access_control_origin_type(OriginAccessControlOriginTypes::S3)
        .signing_behavior(OriginAccessControlSigningBehaviors::Always)
        .signing_protocol(OriginAccessControlSigningProtocols::Sigv4)
        .build()
        .unwrap();

    let updated = cf
        .update_origin_access_control()
        .id(&id)
        .if_match(&etag)
        .origin_access_control_config(new_cfg)
        .send()
        .await
        .expect("update_oac");
    let new_etag = updated.e_tag().expect("etag").to_string();
    assert_ne!(new_etag, etag);

    let list = cf
        .list_origin_access_controls()
        .send()
        .await
        .expect("list_oac");
    let items = list.origin_access_control_list().unwrap();
    assert!(items.quantity() >= 1);

    cf.delete_origin_access_control()
        .id(&id)
        .if_match(&new_etag)
        .send()
        .await
        .expect("delete_oac");

    let err = cf
        .get_origin_access_control()
        .id(&id)
        .send()
        .await
        .unwrap_err();
    assert!(err.into_service_error().is_no_such_origin_access_control());
}

#[tokio::test]
async fn cache_policy_lifecycle_and_managed_seeded() {
    let server = TestServer::start().await;
    let cf = server.cloudfront_client().await;

    let list = cf.list_cache_policies().send().await.expect("list");
    let managed_count = list
        .cache_policy_list()
        .unwrap()
        .items()
        .iter()
        .filter(|i| i.r#type().as_str() == "managed")
        .count();
    assert!(managed_count >= 4, "expected AWS-managed cache policies");

    let cfg = CachePolicyConfig::builder()
        .name("custom-cache")
        .min_ttl(0)
        .max_ttl(86400)
        .default_ttl(3600)
        .parameters_in_cache_key_and_forwarded_to_origin(
            ParametersInCacheKeyAndForwardedToOrigin::builder()
                .enable_accept_encoding_gzip(true)
                .headers_config(
                    CachePolicyHeadersConfig::builder()
                        .header_behavior(CachePolicyHeaderBehavior::None)
                        .build()
                        .unwrap(),
                )
                .cookies_config(
                    CachePolicyCookiesConfig::builder()
                        .cookie_behavior(CachePolicyCookieBehavior::None)
                        .build()
                        .unwrap(),
                )
                .query_strings_config(
                    CachePolicyQueryStringsConfig::builder()
                        .query_string_behavior(CachePolicyQueryStringBehavior::None)
                        .build()
                        .unwrap(),
                )
                .build()
                .unwrap(),
        )
        .build()
        .unwrap();

    let create = cf
        .create_cache_policy()
        .cache_policy_config(cfg)
        .send()
        .await
        .expect("create");
    let id = create.cache_policy().unwrap().id().to_string();
    let etag = create.e_tag().unwrap().to_string();

    let got = cf.get_cache_policy().id(&id).send().await.expect("get");
    assert_eq!(got.cache_policy().unwrap().id(), id);

    cf.delete_cache_policy()
        .id(&id)
        .if_match(&etag)
        .send()
        .await
        .expect("delete");
}

#[tokio::test]
async fn managed_cache_policy_cannot_be_deleted() {
    let server = TestServer::start().await;
    let cf = server.cloudfront_client().await;

    // Trigger seeding by listing first.
    let _ = cf.list_cache_policies().send().await.expect("list");

    let managed_id = "658327ea-f89d-4fab-a63d-7e88639e58f6"; // CachingOptimized
    let got = cf
        .get_cache_policy()
        .id(managed_id)
        .send()
        .await
        .expect("get managed");
    let etag = got.e_tag().unwrap().to_string();

    let res = cf
        .delete_cache_policy()
        .id(managed_id)
        .if_match(&etag)
        .send()
        .await;
    assert!(
        res.unwrap_err().into_service_error().is_illegal_delete(),
        "managed policy must not be deletable"
    );
}

#[tokio::test]
async fn origin_request_policy_lifecycle() {
    let server = TestServer::start().await;
    let cf = server.cloudfront_client().await;

    let cfg = OriginRequestPolicyConfig::builder()
        .name("custom-orp")
        .headers_config(
            OriginRequestPolicyHeadersConfig::builder()
                .header_behavior(OriginRequestPolicyHeaderBehavior::None)
                .build()
                .unwrap(),
        )
        .cookies_config(
            OriginRequestPolicyCookiesConfig::builder()
                .cookie_behavior(OriginRequestPolicyCookieBehavior::None)
                .build()
                .unwrap(),
        )
        .query_strings_config(
            OriginRequestPolicyQueryStringsConfig::builder()
                .query_string_behavior(OriginRequestPolicyQueryStringBehavior::None)
                .build()
                .unwrap(),
        )
        .build()
        .unwrap();

    let create = cf
        .create_origin_request_policy()
        .origin_request_policy_config(cfg)
        .send()
        .await
        .expect("create");
    let id = create.origin_request_policy().unwrap().id().to_string();
    let etag = create.e_tag().unwrap().to_string();

    cf.delete_origin_request_policy()
        .id(&id)
        .if_match(&etag)
        .send()
        .await
        .expect("delete");
}

#[tokio::test]
async fn response_headers_policy_lifecycle() {
    let server = TestServer::start().await;
    let cf = server.cloudfront_client().await;

    let cfg = ResponseHeadersPolicyConfig::builder()
        .name("custom-rhp")
        .cors_config(
            ResponseHeadersPolicyCorsConfig::builder()
                .access_control_allow_credentials(false)
                .access_control_allow_headers(
                    ResponseHeadersPolicyAccessControlAllowHeaders::builder()
                        .quantity(1)
                        .items("*")
                        .build()
                        .unwrap(),
                )
                .access_control_allow_methods(
                    ResponseHeadersPolicyAccessControlAllowMethods::builder()
                        .quantity(1)
                        .items(ResponseHeadersPolicyAccessControlAllowMethodsValues::Get)
                        .build()
                        .unwrap(),
                )
                .access_control_allow_origins(
                    ResponseHeadersPolicyAccessControlAllowOrigins::builder()
                        .quantity(1)
                        .items("*")
                        .build()
                        .unwrap(),
                )
                .origin_override(true)
                .build()
                .unwrap(),
        )
        .build()
        .unwrap();

    let create = cf
        .create_response_headers_policy()
        .response_headers_policy_config(cfg)
        .send()
        .await
        .expect("create");
    let id = create.response_headers_policy().unwrap().id().to_string();
    let etag = create.e_tag().unwrap().to_string();

    cf.delete_response_headers_policy()
        .id(&id)
        .if_match(&etag)
        .send()
        .await
        .expect("delete");
}

#[tokio::test]
async fn continuous_deployment_policy_lifecycle() {
    let server = TestServer::start().await;
    let cf = server.cloudfront_client().await;

    let cfg = ContinuousDeploymentPolicyConfig::builder()
        .staging_distribution_dns_names(
            StagingDistributionDnsNames::builder()
                .quantity(1)
                .items("staging.example.com")
                .build()
                .unwrap(),
        )
        .enabled(true)
        .traffic_config(
            TrafficConfig::builder()
                .r#type(ContinuousDeploymentPolicyType::SingleHeader)
                .single_header_config(
                    ContinuousDeploymentSingleHeaderConfig::builder()
                        .header("aws-cf-cd-staging")
                        .value("true")
                        .build()
                        .unwrap(),
                )
                .build()
                .unwrap(),
        )
        .build()
        .unwrap();

    let create = cf
        .create_continuous_deployment_policy()
        .continuous_deployment_policy_config(cfg)
        .send()
        .await
        .expect("create");
    let id = create
        .continuous_deployment_policy()
        .unwrap()
        .id()
        .to_string();
    let etag = create.e_tag().unwrap().to_string();

    cf.delete_continuous_deployment_policy()
        .id(&id)
        .if_match(&etag)
        .send()
        .await
        .expect("delete");
}

#[tokio::test]
async fn update_oac_with_empty_name_is_rejected() {
    let server = TestServer::start().await;
    let cf = server.cloudfront_client().await;
    let create = cf
        .create_origin_access_control()
        .origin_access_control_config(
            OriginAccessControlConfig::builder()
                .name("oac-validate")
                .origin_access_control_origin_type(OriginAccessControlOriginTypes::S3)
                .signing_behavior(OriginAccessControlSigningBehaviors::Always)
                .signing_protocol(OriginAccessControlSigningProtocols::Sigv4)
                .build()
                .unwrap(),
        )
        .send()
        .await
        .unwrap();
    let id = create.origin_access_control().unwrap().id().to_string();
    let etag = create.e_tag().unwrap().to_string();

    let bad = OriginAccessControlConfig::builder()
        .name("")
        .origin_access_control_origin_type(OriginAccessControlOriginTypes::S3)
        .signing_behavior(OriginAccessControlSigningBehaviors::Always)
        .signing_protocol(OriginAccessControlSigningProtocols::Sigv4)
        .build()
        .unwrap();

    let res = cf
        .update_origin_access_control()
        .id(&id)
        .if_match(&etag)
        .origin_access_control_config(bad)
        .send()
        .await;
    assert!(res.is_err(), "empty Name on update must be rejected");
}
