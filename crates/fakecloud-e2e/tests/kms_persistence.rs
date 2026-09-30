mod helpers;

use aws_sdk_kms::primitives::Blob;
use helpers::TestServer;

/// Key metadata, alias, tags, enabled state, and a round-trippable
/// ciphertext all survive a restart.
#[tokio::test]
async fn persistence_round_trip_key_alias_and_ciphertext() {
    let tmp = tempfile::tempdir().unwrap();
    let mut server = TestServer::start_persistent(tmp.path()).await;
    let kms = server.kms_client().await;

    let created = kms
        .create_key()
        .description("app signing key")
        .send()
        .await
        .unwrap();
    let key_id = created.key_metadata().unwrap().key_id().to_string();
    let key_arn = created.key_metadata().unwrap().arn().unwrap().to_string();

    kms.create_alias()
        .alias_name("alias/app-signing")
        .target_key_id(&key_id)
        .send()
        .await
        .unwrap();

    kms.tag_resource()
        .key_id(&key_id)
        .tags(
            aws_sdk_kms::types::Tag::builder()
                .tag_key("env")
                .tag_value("prod")
                .build()
                .unwrap(),
        )
        .send()
        .await
        .unwrap();

    // Encrypt something while server is up; we'll try to decrypt after restart.
    let plaintext = b"top-secret-payload".to_vec();
    let enc = kms
        .encrypt()
        .key_id(&key_id)
        .plaintext(Blob::new(plaintext.clone()))
        .send()
        .await
        .unwrap();
    let ciphertext = enc.ciphertext_blob().unwrap().as_ref().to_vec();

    server.restart().await;
    let kms = server.kms_client().await;

    // Key survives with its ARN.
    let desc = kms.describe_key().key_id(&key_id).send().await.unwrap();
    let meta = desc.key_metadata().unwrap();
    assert_eq!(meta.arn(), Some(key_arn.as_str()));
    assert_eq!(meta.description(), Some("app signing key"));
    assert!(meta.enabled());

    // Alias survives and still resolves to the same key.
    let aliases = kms.list_aliases().send().await.unwrap();
    let found = aliases
        .aliases()
        .iter()
        .find(|a| a.alias_name() == Some("alias/app-signing"));
    assert!(found.is_some());
    assert_eq!(found.unwrap().target_key_id(), Some(key_id.as_str()));

    // Tag survives.
    let tags = kms
        .list_resource_tags()
        .key_id(&key_id)
        .send()
        .await
        .unwrap();
    assert!(tags
        .tags()
        .iter()
        .any(|t| t.tag_key() == "env" && t.tag_value() == "prod"));

    // Ciphertext from before restart still decrypts.
    let dec = kms
        .decrypt()
        .ciphertext_blob(Blob::new(ciphertext))
        .key_id(&key_id)
        .send()
        .await
        .unwrap();
    assert_eq!(dec.plaintext().unwrap().as_ref(), plaintext.as_slice());
}

/// DisableKey / ScheduleKeyDeletion state is durable across a restart.
#[tokio::test]
async fn persistence_disabled_and_scheduled_deletion_survive_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let mut server = TestServer::start_persistent(tmp.path()).await;
    let kms = server.kms_client().await;

    let key_id = kms
        .create_key()
        .send()
        .await
        .unwrap()
        .key_metadata()
        .unwrap()
        .key_id()
        .to_string();
    kms.disable_key().key_id(&key_id).send().await.unwrap();
    kms.schedule_key_deletion()
        .key_id(&key_id)
        .pending_window_in_days(7)
        .send()
        .await
        .unwrap();

    server.restart().await;
    let kms = server.kms_client().await;

    let meta = kms
        .describe_key()
        .key_id(&key_id)
        .send()
        .await
        .unwrap()
        .key_metadata
        .unwrap();
    assert!(!meta.enabled());
    assert!(meta.deletion_date().is_some());
    assert_eq!(
        meta.key_state(),
        Some(&aws_sdk_kms::types::KeyState::PendingDeletion)
    );
}

/// Key rotation toggled on persists after restart.
#[tokio::test]
async fn persistence_key_rotation_enabled_survives_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let mut server = TestServer::start_persistent(tmp.path()).await;
    let kms = server.kms_client().await;

    let key_id = kms
        .create_key()
        .send()
        .await
        .unwrap()
        .key_metadata()
        .unwrap()
        .key_id()
        .to_string();
    kms.enable_key_rotation()
        .key_id(&key_id)
        .send()
        .await
        .unwrap();

    server.restart().await;
    let kms = server.kms_client().await;

    let status = kms
        .get_key_rotation_status()
        .key_id(&key_id)
        .send()
        .await
        .unwrap();
    assert!(status.key_rotation_enabled());
}

/// Regional aliases survive a restart in their own regions: the same alias
/// name in two regions keeps pointing at each region's key, and each
/// region's ListAliases still pages through only its own aliases.
#[tokio::test]
async fn persistence_regional_aliases_survive_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let mut server = TestServer::start_persistent(tmp.path()).await;
    let mut keys = Vec::new();
    for region in ["us-east-1", "eu-west-1"] {
        let kms = aws_sdk_kms::Client::new(&server.aws_config_in(region).await);
        let key_arn = kms
            .create_key()
            .send()
            .await
            .unwrap()
            .key_metadata()
            .unwrap()
            .arn()
            .unwrap()
            .to_string();
        kms.create_alias()
            .alias_name("alias/shared-name")
            .target_key_id(&key_arn)
            .send()
            .await
            .unwrap();
        keys.push((region, key_arn));
    }

    server.restart().await;

    for (region, key_arn) in &keys {
        let kms = aws_sdk_kms::Client::new(&server.aws_config_in(region).await);
        let resolved = kms
            .describe_key()
            .key_id("alias/shared-name")
            .send()
            .await
            .unwrap()
            .key_metadata()
            .unwrap()
            .arn()
            .unwrap()
            .to_string();
        assert_eq!(&resolved, key_arn, "{region}");
        let listed: Vec<String> = kms
            .list_aliases()
            .send()
            .await
            .unwrap()
            .aliases()
            .iter()
            .filter_map(|a| a.alias_arn().map(str::to_string))
            .filter(|arn| arn.ends_with(":alias/shared-name"))
            .collect();
        assert_eq!(
            listed,
            vec![format!(
                "arn:aws:kms:{region}:123456789012:alias/shared-name"
            )],
            "{region}"
        );
    }
}
