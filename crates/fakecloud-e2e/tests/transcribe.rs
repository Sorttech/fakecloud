//! Amazon Transcribe (transcribe) control-plane E2E.
//!
//! Exercises the transcription-job + vocabulary + tagging lifecycle against a
//! spawned fakecloud server via the AWS Rust SDK, which speaks the real
//! awsJson1.1 wire format (x-amz-target `Transcribe.<Op>`):
//!
//!   StartTranscriptionJob -> GetTranscriptionJob (settles COMPLETED)
//!     -> ListTranscriptionJobs -> CreateVocabulary -> GetVocabulary
//!     -> TagResource / ListTagsForResource -> DeleteVocabulary
//!     -> DeleteTranscriptionJob
//!
//! Honest ASR gap: a completed job carries a well-formed `Transcript` whose
//! `TranscriptFileUri` points at the requested output location, but no
//! transcript JSON is produced there (fakecloud does not run speech
//! recognition). Everything else is real, persisted control-plane state.

mod helpers;

use aws_sdk_transcribe::types::{Media, TranscriptionJobStatus, VocabularyState};
use helpers::TestServer;

async fn transcribe_client(server: &TestServer) -> aws_sdk_transcribe::Client {
    aws_sdk_transcribe::Client::new(&server.aws_config().await)
}

#[tokio::test]
async fn transcription_job_and_vocabulary_lifecycle() {
    let server = TestServer::start().await;
    let tx = transcribe_client(&server).await;

    // StartTranscriptionJob -> QUEUED job with the requested media echoed back.
    let media = Media::builder()
        .media_file_uri("s3://my-input-bucket/meeting.wav")
        .build();
    let started = tx
        .start_transcription_job()
        .transcription_job_name("e2e-job")
        .language_code(aws_sdk_transcribe::types::LanguageCode::EnUs)
        .media(media)
        .output_bucket_name("my-output-bucket")
        .send()
        .await
        .expect("start transcription job");
    let job = started.transcription_job().expect("job");
    assert_eq!(job.transcription_job_name(), Some("e2e-job"));
    assert_eq!(
        job.media().and_then(|m| m.media_file_uri()),
        Some("s3://my-input-bucket/meeting.wav")
    );

    // Duplicate name -> ConflictException.
    let dup = tx
        .start_transcription_job()
        .transcription_job_name("e2e-job")
        .media(Media::builder().media_file_uri("s3://b/a.wav").build())
        .send()
        .await;
    assert!(dup.is_err(), "duplicate job name should conflict");

    // GetTranscriptionJob settles the job to COMPLETED with a Transcript.
    let got = tx
        .get_transcription_job()
        .transcription_job_name("e2e-job")
        .send()
        .await
        .expect("get transcription job");
    let gjob = got.transcription_job().expect("job");
    assert_eq!(
        gjob.transcription_job_status(),
        Some(&TranscriptionJobStatus::Completed)
    );
    let uri = gjob
        .transcript()
        .and_then(|t| t.transcript_file_uri())
        .expect("transcript file uri");
    assert!(
        uri.contains("my-output-bucket"),
        "transcript uri should point at the output bucket: {uri}"
    );

    // ListTranscriptionJobs sees it.
    let listed = tx
        .list_transcription_jobs()
        .send()
        .await
        .expect("list transcription jobs");
    assert!(
        listed
            .transcription_job_summaries()
            .iter()
            .any(|s| s.transcription_job_name() == Some("e2e-job")),
        "job should appear in ListTranscriptionJobs"
    );

    // CreateVocabulary -> PENDING; GetVocabulary settles it to READY.
    tx.create_vocabulary()
        .vocabulary_name("e2e-vocab")
        .language_code(aws_sdk_transcribe::types::LanguageCode::EnUs)
        .phrases("Amazon")
        .phrases("Transcribe")
        .send()
        .await
        .expect("create vocabulary");
    let gv = tx
        .get_vocabulary()
        .vocabulary_name("e2e-vocab")
        .send()
        .await
        .expect("get vocabulary");
    assert_eq!(gv.vocabulary_state(), Some(&VocabularyState::Ready));
    assert!(gv.download_uri().is_some(), "vocabulary has a download URI");

    // Tag the vocabulary and read the tags back.
    let arn = "arn:aws:transcribe:us-east-1:000000000000:vocabulary/e2e-vocab".to_string();
    tx.tag_resource()
        .resource_arn(&arn)
        .tags(
            aws_sdk_transcribe::types::Tag::builder()
                .key("team")
                .value("asr")
                .build()
                .unwrap(),
        )
        .send()
        .await
        .expect("tag resource");
    let tags = tx
        .list_tags_for_resource()
        .resource_arn(&arn)
        .send()
        .await
        .expect("list tags");
    assert!(
        tags.tags()
            .iter()
            .any(|t| t.key() == "team" && t.value() == "asr"),
        "tag should be present"
    );

    // DeleteVocabulary -> subsequent GetVocabulary 404s.
    tx.delete_vocabulary()
        .vocabulary_name("e2e-vocab")
        .send()
        .await
        .expect("delete vocabulary");
    let after = tx
        .get_vocabulary()
        .vocabulary_name("e2e-vocab")
        .send()
        .await;
    assert!(after.is_err(), "deleted vocabulary should not be found");

    // DeleteTranscriptionJob is idempotent and removes the job.
    tx.delete_transcription_job()
        .transcription_job_name("e2e-job")
        .send()
        .await
        .expect("delete transcription job");
    let gone = tx
        .get_transcription_job()
        .transcription_job_name("e2e-job")
        .send()
        .await;
    assert!(gone.is_err(), "deleted job should not be found");
}

/// UpdateLanguageModel is newer than the typed aws-sdk-transcribe client, so
/// drive it over raw awsJson1.1 (x-amz-target `Transcribe.<Op>`), the same wire
/// format the SDK uses.
#[tokio::test]
async fn update_language_model_re_encrypts_a_settled_model() {
    let server = TestServer::start().await;
    let tx = transcribe_client(&server).await;

    tx.create_language_model()
        .model_name("e2e-clm")
        .language_code(aws_sdk_transcribe::types::ClmLanguageCode::EnUs)
        .base_model_name(aws_sdk_transcribe::types::BaseModelName::WideBand)
        .input_data_config(
            aws_sdk_transcribe::types::InputDataConfig::builder()
                .s3_uri("s3://training/data/")
                .data_access_role_arn("arn:aws:iam::000000000000:role/old")
                .build()
                .unwrap(),
        )
        .send()
        .await
        .expect("create language model");

    let auth = "AWS4-HMAC-SHA256 Credential=test/20240101/us-east-1/transcribe/aws4_request, SignedHeaders=host, Signature=0";
    let call = |op: &str, body: String| {
        let url = server.endpoint().to_string();
        let target = format!("Transcribe.{op}");
        async move {
            reqwest::Client::new()
                .post(url)
                .header("Authorization", auth)
                .header("Content-Type", "application/x-amz-json-1.1")
                .header("X-Amz-Target", target)
                .body(body)
                .send()
                .await
                .expect("request")
        }
    };

    // Training has not settled yet, so AWS rejects the update with a conflict.
    let resp = call(
        "UpdateLanguageModel",
        r#"{"ModelName":"e2e-clm","DataAccessRoleArn":"arn:aws:iam::000000000000:role/new"}"#
            .to_string(),
    )
    .await;
    assert_eq!(
        resp.status(),
        409,
        "updating an IN_PROGRESS model should conflict"
    );
    let err: serde_json::Value = resp.json().await.unwrap();
    assert!(
        err["__type"]
            .as_str()
            .unwrap_or_default()
            .contains("Conflict"),
        "expected ConflictException: {err}"
    );

    // DescribeLanguageModel settles the model to COMPLETED.
    let described = tx
        .describe_language_model()
        .model_name("e2e-clm")
        .send()
        .await
        .expect("describe language model");
    assert_eq!(
        described.language_model().and_then(|m| m.model_status()),
        Some(&aws_sdk_transcribe::types::ModelStatus::Completed)
    );

    // The update now succeeds and re-points both the KMS key and the role.
    let resp = call(
        "UpdateLanguageModel",
        r#"{"ModelName":"e2e-clm","DataAccessRoleArn":"arn:aws:iam::000000000000:role/new","EncryptionConfiguration":{"KmsKeyId":"alias/clm"}}"#
            .to_string(),
    )
    .await;
    assert!(resp.status().is_success(), "update: {}", resp.status());
    let updated: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(updated["ModelName"], "e2e-clm");
    assert_eq!(updated["ModelStatus"], "COMPLETED");
    assert!(
        updated["LastModifiedTime"].is_number(),
        "update returns a last-modified timestamp: {updated}"
    );

    // The new role and key are readable back through DescribeLanguageModel.
    let resp = call(
        "DescribeLanguageModel",
        r#"{"ModelName":"e2e-clm"}"#.to_string(),
    )
    .await;
    let described: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(
        described["LanguageModel"]["InputDataConfig"]["DataAccessRoleArn"],
        "arn:aws:iam::000000000000:role/new"
    );
    assert_eq!(
        described["LanguageModel"]["EncryptionConfiguration"]["KmsKeyId"],
        "alias/clm"
    );
    assert_eq!(
        described["LanguageModel"]["InputDataConfig"]["S3Uri"], "s3://training/data/",
        "the update leaves the training data in place"
    );

    // An unknown model is a not-found, not a silent no-op.
    let resp = call(
        "UpdateLanguageModel",
        r#"{"ModelName":"missing-clm"}"#.to_string(),
    )
    .await;
    assert_eq!(resp.status(), 404);
    let err: serde_json::Value = resp.json().await.unwrap();
    assert!(
        err["__type"]
            .as_str()
            .unwrap_or_default()
            .contains("NotFound"),
        "expected NotFoundException: {err}"
    );
}

/// `EncryptionConfiguration` joined the vocabulary create/update requests and
/// the `GetVocabulary` read shape in a model refresh, ahead of the typed
/// aws-sdk-transcribe client, so drive those over raw awsJson1.1.
#[tokio::test]
async fn vocabulary_round_trips_its_encryption_configuration() {
    let server = TestServer::start().await;

    let auth = "AWS4-HMAC-SHA256 Credential=test/20240101/us-east-1/transcribe/aws4_request, SignedHeaders=host, Signature=0";
    let call = |op: &str, body: String| {
        let url = server.endpoint().to_string();
        let target = format!("Transcribe.{op}");
        async move {
            reqwest::Client::new()
                .post(url)
                .header("Authorization", auth)
                .header("Content-Type", "application/x-amz-json-1.1")
                .header("X-Amz-Target", target)
                .body(body)
                .send()
                .await
                .expect("request")
        }
    };

    let resp = call(
        "CreateVocabulary",
        r#"{"VocabularyName":"enc-vocab","LanguageCode":"en-US","Phrases":["Amazon"],"DataAccessRoleArn":"arn:aws:iam::000000000000:role/vocab","EncryptionConfiguration":{"KmsKeyId":"alias/vocab"}}"#
            .to_string(),
    )
    .await;
    assert!(resp.status().is_success(), "create: {}", resp.status());

    let resp = call(
        "GetVocabulary",
        r#"{"VocabularyName":"enc-vocab"}"#.to_string(),
    )
    .await;
    let got: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(
        got["DataAccessRoleArn"], "arn:aws:iam::000000000000:role/vocab",
        "vocabulary: {got}"
    );
    assert_eq!(
        got["EncryptionConfiguration"]["KmsKeyId"], "alias/vocab",
        "vocabulary: {got}"
    );

    // An update repoints the key.
    let resp = call(
        "UpdateVocabulary",
        r#"{"VocabularyName":"enc-vocab","LanguageCode":"en-US","Phrases":["Amazon"],"EncryptionConfiguration":{"KmsKeyId":"alias/rotated"}}"#
            .to_string(),
    )
    .await;
    assert!(resp.status().is_success(), "update: {}", resp.status());
    let resp = call(
        "GetVocabulary",
        r#"{"VocabularyName":"enc-vocab"}"#.to_string(),
    )
    .await;
    let got: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(
        got["EncryptionConfiguration"]["KmsKeyId"], "alias/rotated",
        "vocabulary: {got}"
    );

    // Vocabulary filters carry the same pair.
    let resp = call(
        "CreateVocabularyFilter",
        r#"{"VocabularyFilterName":"enc-filter","LanguageCode":"en-US","Words":["nope"],"DataAccessRoleArn":"arn:aws:iam::000000000000:role/filter","EncryptionConfiguration":{"KmsKeyId":"alias/filter"}}"#
            .to_string(),
    )
    .await;
    assert!(
        resp.status().is_success(),
        "create filter: {}",
        resp.status()
    );
    let resp = call(
        "GetVocabularyFilter",
        r#"{"VocabularyFilterName":"enc-filter"}"#.to_string(),
    )
    .await;
    let got: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(
        got["DataAccessRoleArn"], "arn:aws:iam::000000000000:role/filter",
        "filter: {got}"
    );
    assert_eq!(
        got["EncryptionConfiguration"]["KmsKeyId"], "alias/filter",
        "filter: {got}"
    );
}
