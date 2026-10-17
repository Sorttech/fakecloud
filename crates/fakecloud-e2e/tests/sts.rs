// bug-audit 2026-05-28, 5.4: AssumeRole targeting a role that does not exist
// must be denied, not silently minted credentials.
mod helpers;

use aws_sdk_sts::error::SdkError;
use helpers::TestServer;

#[tokio::test]
async fn sts_assume_role_nonexistent_role_is_denied() {
    let server = TestServer::start().await;
    let sts = server.sts_client().await;

    let err = sts
        .assume_role()
        .role_arn("arn:aws:iam::123456789012:role/this-role-does-not-exist-5p4")
        .role_session_name("session")
        .send()
        .await
        .expect_err("assuming a non-existent role must be denied");
    let code = match &err {
        SdkError::ServiceError(e) => e.err().meta().code().map(str::to_string),
        _ => None,
    };
    assert_eq!(code.as_deref(), Some("AccessDenied"), "err: {err:?}");
}

// AssumeRoot into another account without centralized root access is denied
// with STS's `AccessDenied` code (the query-protocol code STS uses for every
// authorization failure), not the JSON-style `AccessDeniedException`.
#[tokio::test]
async fn sts_assume_root_cross_account_is_access_denied() {
    let server = TestServer::start().await;
    let sts = server.sts_client().await;

    let err = sts
        .assume_root()
        .target_principal("111122223333")
        .task_policy_arn(
            aws_sdk_sts::types::PolicyDescriptorType::builder()
                .arn("arn:aws:iam::aws:policy/root-task/S3UnlockBucketPolicy")
                .build(),
        )
        .duration_seconds(900)
        .send()
        .await
        .expect_err("cross-account AssumeRoot without RootSessions must be denied");
    let code = match &err {
        SdkError::ServiceError(e) => e.err().meta().code().map(str::to_string),
        _ => None,
    };
    assert_eq!(code.as_deref(), Some("AccessDenied"), "err: {err:?}");
}
