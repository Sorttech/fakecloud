//! CloudFront Batch 5 E2E: Field-Level Encryption + Realtime Log Configs.

mod helpers;

use aws_sdk_cloudfront::types::{
    ContentTypeProfileConfig, ContentTypeProfiles, EncryptionEntities, EndPoint,
    FieldLevelEncryptionConfig, FieldLevelEncryptionProfileConfig, FieldPatterns,
    KinesisStreamConfig, QueryArgProfileConfig, QueryArgProfiles,
};
use helpers::TestServer;

fn fle_cfg(caller_ref: &str, profile_id: &str) -> FieldLevelEncryptionConfig {
    FieldLevelEncryptionConfig::builder()
        .caller_reference(caller_ref)
        .comment("e2e fle config")
        .query_arg_profile_config(
            QueryArgProfileConfig::builder()
                .forward_when_query_arg_profile_is_unknown(true)
                .query_arg_profiles(QueryArgProfiles::builder().quantity(0).build().unwrap())
                .build()
                .unwrap(),
        )
        .content_type_profile_config(
            ContentTypeProfileConfig::builder()
                .forward_when_content_type_is_unknown(true)
                .content_type_profiles(
                    ContentTypeProfiles::builder()
                        .quantity(1)
                        .items(
                            aws_sdk_cloudfront::types::ContentTypeProfile::builder()
                                .format(aws_sdk_cloudfront::types::Format::UrlEncoded)
                                .profile_id(profile_id)
                                .content_type("application/x-www-form-urlencoded")
                                .build()
                                .unwrap(),
                        )
                        .build()
                        .unwrap(),
                )
                .build()
                .unwrap(),
        )
        .build()
        .unwrap()
}

fn fle_profile_cfg(
    name: &str,
    caller_ref: &str,
    public_key_id: &str,
) -> FieldLevelEncryptionProfileConfig {
    FieldLevelEncryptionProfileConfig::builder()
        .name(name)
        .caller_reference(caller_ref)
        .comment("e2e fle profile")
        .encryption_entities(
            EncryptionEntities::builder()
                .quantity(1)
                .items(
                    aws_sdk_cloudfront::types::EncryptionEntity::builder()
                        .public_key_id(public_key_id)
                        .provider_id("provider-1")
                        .field_patterns(
                            FieldPatterns::builder()
                                .quantity(1)
                                .items("ssn")
                                .build()
                                .unwrap(),
                        )
                        .build()
                        .unwrap(),
                )
                .build()
                .unwrap(),
        )
        .build()
        .unwrap()
}

#[tokio::test]
async fn fle_profile_lifecycle() {
    let server = TestServer::start().await;
    let cf = server.cloudfront_client().await;

    let create = cf
        .create_field_level_encryption_profile()
        .field_level_encryption_profile_config(fle_profile_cfg(
            "e2e-prof",
            "e2e-prof-ref",
            "K12345",
        ))
        .send()
        .await
        .expect("create profile");
    let prof = create.field_level_encryption_profile().expect("prof");
    let id = prof.id().to_string();
    let etag = create.e_tag().unwrap().to_string();

    cf.get_field_level_encryption_profile()
        .id(&id)
        .send()
        .await
        .expect("get profile");

    cf.get_field_level_encryption_profile_config()
        .id(&id)
        .send()
        .await
        .expect("get profile config");

    let upd = cf
        .update_field_level_encryption_profile()
        .id(&id)
        .if_match(&etag)
        .field_level_encryption_profile_config(fle_profile_cfg(
            "e2e-prof",
            "e2e-prof-ref",
            "K67890",
        ))
        .send()
        .await
        .expect("update");
    let new_etag = upd.e_tag().unwrap().to_string();

    let list = cf
        .list_field_level_encryption_profiles()
        .send()
        .await
        .expect("list");
    assert!(
        list.field_level_encryption_profile_list()
            .unwrap()
            .quantity()
            >= 1
    );

    cf.delete_field_level_encryption_profile()
        .id(&id)
        .if_match(&new_etag)
        .send()
        .await
        .expect("delete");
}

#[tokio::test]
async fn fle_config_lifecycle() {
    let server = TestServer::start().await;
    let cf = server.cloudfront_client().await;

    // Profile first (referenced by FLE config).
    let p = cf
        .create_field_level_encryption_profile()
        .field_level_encryption_profile_config(fle_profile_cfg(
            "e2e-fle-prof",
            "e2e-fle-prof-ref",
            "K-fle",
        ))
        .send()
        .await
        .unwrap();
    let pid = p.field_level_encryption_profile().unwrap().id().to_string();

    let create = cf
        .create_field_level_encryption_config()
        .field_level_encryption_config(fle_cfg("e2e-fle-1", &pid))
        .send()
        .await
        .expect("create fle");
    let fle = create.field_level_encryption().expect("fle");
    let id = fle.id().to_string();
    let etag = create.e_tag().unwrap().to_string();

    cf.get_field_level_encryption()
        .id(&id)
        .send()
        .await
        .expect("get");
    cf.get_field_level_encryption_config()
        .id(&id)
        .send()
        .await
        .expect("get config");

    let upd = cf
        .update_field_level_encryption_config()
        .id(&id)
        .if_match(&etag)
        .field_level_encryption_config(fle_cfg("e2e-fle-1", &pid))
        .send()
        .await
        .expect("update");
    let new_etag = upd.e_tag().unwrap().to_string();

    let list = cf
        .list_field_level_encryption_configs()
        .send()
        .await
        .unwrap();
    assert!(list.field_level_encryption_list().unwrap().quantity() >= 1);

    cf.delete_field_level_encryption_config()
        .id(&id)
        .if_match(&new_etag)
        .send()
        .await
        .expect("delete");
}

#[tokio::test]
async fn duplicate_fle_caller_reference_is_rejected() {
    let server = TestServer::start().await;
    let cf = server.cloudfront_client().await;
    let p = cf
        .create_field_level_encryption_profile()
        .field_level_encryption_profile_config(fle_profile_cfg("dup-prof", "dup-prof-ref", "Kdup"))
        .send()
        .await
        .unwrap();
    let pid = p.field_level_encryption_profile().unwrap().id().to_string();
    cf.create_field_level_encryption_config()
        .field_level_encryption_config(fle_cfg("dup-fle", &pid))
        .send()
        .await
        .unwrap();
    let res = cf
        .create_field_level_encryption_config()
        .field_level_encryption_config(fle_cfg("dup-fle", &pid))
        .send()
        .await;
    assert!(
        res.unwrap_err()
            .into_service_error()
            .is_field_level_encryption_config_already_exists(),
        "duplicate FLE CallerReference must be rejected"
    );
}

fn rtl_endpoint() -> EndPoint {
    EndPoint::builder()
        .stream_type("Kinesis")
        .kinesis_stream_config(
            KinesisStreamConfig::builder()
                .role_arn("arn:aws:iam::000000000000:role/cf-rtl")
                .stream_arn("arn:aws:kinesis:us-east-1:000000000000:stream/cf-rtl")
                .build()
                .unwrap(),
        )
        .build()
        .unwrap()
}

#[tokio::test]
async fn realtime_log_config_lifecycle() {
    let server = TestServer::start().await;
    let cf = server.cloudfront_client().await;

    let create = cf
        .create_realtime_log_config()
        .name("e2e-rtl-1")
        .sampling_rate(50)
        .end_points(rtl_endpoint())
        .fields("timestamp")
        .fields("c-ip")
        .send()
        .await
        .expect("create");
    let cfg = create.realtime_log_config().expect("cfg");
    let arn = cfg.arn().to_string();
    assert!(arn.contains(":realtime-log-config/e2e-rtl-1"));

    cf.get_realtime_log_config()
        .name("e2e-rtl-1")
        .send()
        .await
        .expect("get by name");
    cf.get_realtime_log_config()
        .arn(&arn)
        .send()
        .await
        .expect("get by arn");

    cf.update_realtime_log_config()
        .name("e2e-rtl-1")
        .arn(&arn)
        .sampling_rate(75)
        .end_points(rtl_endpoint())
        .fields("timestamp")
        .send()
        .await
        .expect("update");

    let list = cf.list_realtime_log_configs().send().await.expect("list");
    let items = list.realtime_log_configs().unwrap().items();
    assert!(!items.is_empty());

    cf.delete_realtime_log_config()
        .name("e2e-rtl-1")
        .send()
        .await
        .expect("delete by name");
}

#[tokio::test]
async fn duplicate_realtime_log_config_is_rejected() {
    let server = TestServer::start().await;
    let cf = server.cloudfront_client().await;
    cf.create_realtime_log_config()
        .name("e2e-rtl-dup")
        .sampling_rate(10)
        .end_points(rtl_endpoint())
        .fields("timestamp")
        .send()
        .await
        .unwrap();
    let res = cf
        .create_realtime_log_config()
        .name("e2e-rtl-dup")
        .sampling_rate(10)
        .end_points(rtl_endpoint())
        .fields("timestamp")
        .send()
        .await;
    assert!(
        res.unwrap_err()
            .into_service_error()
            .is_realtime_log_config_already_exists(),
        "duplicate name must be rejected"
    );
}
