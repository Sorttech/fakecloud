//! CloudFront Batch 4 E2E: legacy Streaming Distributions (RTMP).
#![allow(deprecated)]

mod helpers;

use aws_sdk_cloudfront::types::{
    PriceClass, S3Origin, StreamingDistributionConfig, TrustedSigners,
};
use helpers::TestServer;

fn streaming_cfg(caller_ref: &str, enabled: bool) -> StreamingDistributionConfig {
    StreamingDistributionConfig::builder()
        .caller_reference(caller_ref)
        .s3_origin(
            S3Origin::builder()
                .domain_name("origin.example.com.s3.amazonaws.com")
                .origin_access_identity("")
                .build()
                .unwrap(),
        )
        .comment("e2e streaming")
        .price_class(PriceClass::PriceClassAll)
        .enabled(enabled)
        .trusted_signers(
            TrustedSigners::builder()
                .enabled(false)
                .quantity(0)
                .build()
                .unwrap(),
        )
        .build()
        .unwrap()
}

#[tokio::test]
async fn streaming_distribution_lifecycle() {
    let server = TestServer::start().await;
    let cf = server.cloudfront_client().await;

    let create = cf
        .create_streaming_distribution()
        .streaming_distribution_config(streaming_cfg("e2e-sd-1", true))
        .send()
        .await
        .expect("create");
    let dist = create.streaming_distribution().expect("dist");
    let id = dist.id().to_string();
    let etag = create.e_tag().unwrap().to_string();
    assert!(dist.domain_name().ends_with(".cloudfront.net"));
    assert!(dist.arn().contains(":streaming-distribution/"));

    let _ = cf
        .get_streaming_distribution()
        .id(&id)
        .send()
        .await
        .expect("get");

    let _ = cf
        .get_streaming_distribution_config()
        .id(&id)
        .send()
        .await
        .expect("getconfig");

    // Disable then delete (DeleteStreamingDistribution requires Enabled=false).
    let upd = cf
        .update_streaming_distribution()
        .id(&id)
        .if_match(&etag)
        .streaming_distribution_config(streaming_cfg("e2e-sd-1", false))
        .send()
        .await
        .expect("update");
    let new_etag = upd.e_tag().unwrap().to_string();

    cf.delete_streaming_distribution()
        .id(&id)
        .if_match(&new_etag)
        .send()
        .await
        .expect("delete");
}

#[tokio::test]
async fn delete_enabled_streaming_distribution_is_rejected() {
    let server = TestServer::start().await;
    let cf = server.cloudfront_client().await;
    let create = cf
        .create_streaming_distribution()
        .streaming_distribution_config(streaming_cfg("e2e-sd-2", true))
        .send()
        .await
        .unwrap();
    let id = create.streaming_distribution().unwrap().id().to_string();
    let etag = create.e_tag().unwrap().to_string();
    let res = cf
        .delete_streaming_distribution()
        .id(&id)
        .if_match(&etag)
        .send()
        .await;
    assert!(
        res.unwrap_err()
            .into_service_error()
            .is_streaming_distribution_not_disabled(),
        "deleting enabled streaming dist must fail"
    );
}

#[tokio::test]
async fn duplicate_streaming_caller_reference_is_rejected() {
    let server = TestServer::start().await;
    let cf = server.cloudfront_client().await;
    cf.create_streaming_distribution()
        .streaming_distribution_config(streaming_cfg("e2e-sd-dup", true))
        .send()
        .await
        .unwrap();
    let res = cf
        .create_streaming_distribution()
        .streaming_distribution_config(streaming_cfg("e2e-sd-dup", true))
        .send()
        .await;
    assert!(
        res.unwrap_err()
            .into_service_error()
            .is_streaming_distribution_already_exists(),
        "duplicate CallerReference must be rejected"
    );
}

#[tokio::test]
async fn list_streaming_distributions_includes_created() {
    let server = TestServer::start().await;
    let cf = server.cloudfront_client().await;
    cf.create_streaming_distribution()
        .streaming_distribution_config(streaming_cfg("e2e-sd-list", true))
        .send()
        .await
        .unwrap();
    let list = cf.list_streaming_distributions().send().await.unwrap();
    assert!(
        list.streaming_distribution_list().unwrap().quantity() >= 1,
        "list should contain at least one streaming distribution"
    );
}

#[tokio::test]
async fn delete_streaming_distribution_clears_tags() {
    use aws_sdk_cloudfront::types::{
        StreamingDistributionConfigWithTags, Tag as SdkTag, Tags as SdkTags,
    };
    let server = TestServer::start().await;
    let cf = server.cloudfront_client().await;

    let create = cf
        .create_streaming_distribution_with_tags()
        .streaming_distribution_config_with_tags(
            StreamingDistributionConfigWithTags::builder()
                .streaming_distribution_config(streaming_cfg("e2e-sd-tagcleanup", false))
                .tags(
                    SdkTags::builder()
                        .items(SdkTag::builder().key("env").value("e2e").build().unwrap())
                        .build(),
                )
                .build(),
        )
        .send()
        .await
        .unwrap();
    let dist = create.streaming_distribution().unwrap();
    let id = dist.id().to_string();
    let arn = dist.arn().to_string();
    let etag = create.e_tag().unwrap().to_string();

    // Confirm tags exist via ListTagsForResource.
    let tags = cf
        .list_tags_for_resource()
        .resource(&arn)
        .send()
        .await
        .unwrap();
    assert_eq!(tags.tags().unwrap().items().len(), 1);

    cf.delete_streaming_distribution()
        .id(&id)
        .if_match(&etag)
        .send()
        .await
        .unwrap();

    // After delete, tags must be gone.
    let tags = cf
        .list_tags_for_resource()
        .resource(&arn)
        .send()
        .await
        .unwrap();
    assert_eq!(tags.tags().unwrap().items().len(), 0);
}
