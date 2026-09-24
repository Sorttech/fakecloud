//! End-to-end SCP enforcement tests.
//!
//! Drives real `aws-sdk-s3` + `aws-sdk-sqs` against fakecloud running
//! in `FAKECLOUD_IAM=strict`, with an organization and SCPs attached.
//! Covers the Batch 4 behavior contract:
//!
//! - Off by default (no org, `FAKECLOUD_IAM=strict`): no behavior change.
//! - Management account is always exempt from SCP enforcement.
//! - A custom SCP denying a service blocks member accounts.
//! - Detaching `FullAWSAccess` from root + attaching a restrictive
//!   SCP demonstrates allow-list ceiling semantics.

mod helpers;

use aws_credential_types::Credentials;
use aws_sdk_organizations::Client as OrgsClient;
use aws_sdk_s3::Client as S3Client;
use aws_sdk_sqs::Client as SqsClient;
use helpers::TestServer;

const ACCOUNT_A: &str = "111111111111"; // management
const ACCOUNT_B: &str = "222222222222"; // member

async fn start() -> TestServer {
    TestServer::start_with_env(&[
        ("FAKECLOUD_IAM", "strict"),
        ("FAKECLOUD_VERIFY_SIGV4", "true"),
        ("FAKECLOUD_CONTAINER_CLI", "false"),
    ])
    .await
}

async fn config_with(server: &TestServer, akid: &str, secret: &str) -> aws_config::SdkConfig {
    aws_config::defaults(aws_config::BehaviorVersion::latest())
        .endpoint_url(server.endpoint())
        .region(aws_config::Region::new("us-east-1"))
        .credentials_provider(Credentials::new(akid, secret, None, None, "scp-e2e"))
        .load()
        .await
}

/// Create the organization and return its id, which member bootstraps
/// pass to `create_admin_in_org` to enroll into it.
async fn create_org(orgs: &OrgsClient) -> String {
    orgs.create_organization()
        .send()
        .await
        .unwrap()
        .organization()
        .unwrap()
        .id()
        .unwrap()
        .to_string()
}

const SCP_ALLOW_ALL: &str =
    r#"{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Action":"*","Resource":"*"}]}"#;

const SCP_DENY_SQS: &str = r#"{"Version":"2012-10-17","Statement":[
        {"Effect":"Allow","Action":"*","Resource":"*"},
        {"Effect":"Deny","Action":"sqs:*","Resource":"*"}
    ]}"#;

#[tokio::test]
async fn member_account_blocked_by_explicit_deny_scp() {
    let server = start().await;
    let (a_akid, a_secret) = server.create_admin(ACCOUNT_A, "admin-a").await;
    let a_cfg = config_with(&server, &a_akid, &a_secret).await;
    let orgs = OrgsClient::new(&a_cfg);

    // Management = A.
    let org_id = create_org(&orgs).await;
    let root_id = orgs.list_roots().send().await.unwrap().roots()[0]
        .id()
        .unwrap()
        .to_string();

    // Account B is bootstrapped as a member so SCPs apply to it.
    let (b_akid, b_secret) = server
        .create_admin_in_org(ACCOUNT_B, "admin-b", &org_id)
        .await;
    let b_cfg = config_with(&server, &b_akid, &b_secret).await;

    // Baseline: before SCP attached, B can use SQS.
    let sqs_b = SqsClient::new(&b_cfg);
    sqs_b
        .create_queue()
        .queue_name("pre-scp-queue")
        .send()
        .await
        .unwrap();

    // Attach custom SCP that denies sqs:* to root.
    let policy = orgs
        .create_policy()
        .name("DenySqs")
        .description("")
        .r#type(aws_sdk_organizations::types::PolicyType::ServiceControlPolicy)
        .content(SCP_DENY_SQS)
        .send()
        .await
        .unwrap();
    let policy_id = policy
        .policy()
        .unwrap()
        .policy_summary()
        .unwrap()
        .id()
        .unwrap()
        .to_string();
    orgs.attach_policy()
        .policy_id(&policy_id)
        .target_id(&root_id)
        .send()
        .await
        .unwrap();

    // Member B is now blocked from SQS.
    let err = sqs_b
        .create_queue()
        .queue_name("post-scp-queue")
        .send()
        .await
        .unwrap_err();
    let msg = format!("{err:?}");
    assert!(
        msg.contains("AccessDenied") || msg.contains("403"),
        "expected AccessDenied, got: {msg}"
    );

    // S3 still works for B — SCP only denies sqs:*.
    let s3_b = S3Client::new(&b_cfg);
    s3_b.create_bucket()
        .bucket("scp-allowed-bucket")
        .send()
        .await
        .unwrap();
}

#[tokio::test]
async fn management_account_exempt_from_scp() {
    let server = start().await;
    let (a_akid, a_secret) = server.create_admin(ACCOUNT_A, "admin-a").await;
    let a_cfg = config_with(&server, &a_akid, &a_secret).await;
    let orgs = OrgsClient::new(&a_cfg);

    orgs.create_organization().send().await.unwrap();
    let root_id = orgs.list_roots().send().await.unwrap().roots()[0]
        .id()
        .unwrap()
        .to_string();

    // Deny-all SCP attached to root.
    let policy = orgs
        .create_policy()
        .name("DenyEverything")
        .description("")
        .r#type(aws_sdk_organizations::types::PolicyType::ServiceControlPolicy)
        .content(
            r#"{"Version":"2012-10-17","Statement":[
                {"Effect":"Deny","Action":"*","Resource":"*"}
            ]}"#,
        )
        .send()
        .await
        .unwrap();
    let policy_id = policy
        .policy()
        .unwrap()
        .policy_summary()
        .unwrap()
        .id()
        .unwrap()
        .to_string();
    orgs.attach_policy()
        .policy_id(&policy_id)
        .target_id(&root_id)
        .send()
        .await
        .unwrap();

    // Management account A can still use SQS even with DenyAll SCP
    // attached to root — AWS always exempts the management account.
    let sqs_a = SqsClient::new(&a_cfg);
    sqs_a
        .create_queue()
        .queue_name("management-immune")
        .send()
        .await
        .unwrap();
}

#[tokio::test]
async fn detach_full_aws_access_then_custom_scp_controls_ceiling() {
    let server = start().await;
    let (a_akid, a_secret) = server.create_admin(ACCOUNT_A, "admin-a").await;
    let a_cfg = config_with(&server, &a_akid, &a_secret).await;
    let orgs = OrgsClient::new(&a_cfg);

    let org_id = create_org(&orgs).await;
    let root_id = orgs.list_roots().send().await.unwrap().roots()[0]
        .id()
        .unwrap()
        .to_string();

    let (b_akid, b_secret) = server
        .create_admin_in_org(ACCOUNT_B, "admin-b", &org_id)
        .await;
    let b_cfg = config_with(&server, &b_akid, &b_secret).await;
    let sqs_b = SqsClient::new(&b_cfg);
    let s3_b = S3Client::new(&b_cfg);

    // Detach FullAWSAccess from root. Now no SCP allow-all reaches B.
    orgs.detach_policy()
        .policy_id("p-FullAWSAccess")
        .target_id(&root_id)
        .send()
        .await
        .unwrap();

    // B cannot do anything that flows through an SCP-enforced service.
    let err = s3_b
        .create_bucket()
        .bucket("should-be-denied")
        .send()
        .await
        .unwrap_err();
    assert!(format!("{err:?}").contains("AccessDenied") || format!("{err:?}").contains("403"));

    // Attach an SCP that allows only SQS. S3 stays denied; SQS works.
    let policy = orgs
        .create_policy()
        .name("AllowSqsOnly")
        .description("")
        .r#type(aws_sdk_organizations::types::PolicyType::ServiceControlPolicy)
        .content(
            r#"{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Action":"sqs:*","Resource":"*"}]}"#,
        )
        .send()
        .await
        .unwrap();
    let policy_id = policy
        .policy()
        .unwrap()
        .policy_summary()
        .unwrap()
        .id()
        .unwrap()
        .to_string();
    orgs.attach_policy()
        .policy_id(&policy_id)
        .target_id(&root_id)
        .send()
        .await
        .unwrap();

    sqs_b
        .create_queue()
        .queue_name("sqs-allowed")
        .send()
        .await
        .unwrap();
    let err = s3_b
        .create_bucket()
        .bucket("still-denied")
        .send()
        .await
        .unwrap_err();
    assert!(format!("{err:?}").contains("AccessDenied") || format!("{err:?}").contains("403"));

    // Re-attach FullAWSAccess and detach the restrictive SCP — B's
    // ceiling is now allow-all again. AWS intersects SCPs across the
    // chain: with both `FullAWSAccess` (allow *) and `AllowSqsOnly`
    // attached, the intersection still caps at sqs only, so S3 stays
    // denied until the narrow SCP comes off.
    orgs.attach_policy()
        .policy_id("p-FullAWSAccess")
        .target_id(&root_id)
        .send()
        .await
        .unwrap();
    orgs.detach_policy()
        .policy_id(&policy_id)
        .target_id(&root_id)
        .send()
        .await
        .unwrap();
    s3_b.create_bucket()
        .bucket("open-again")
        .send()
        .await
        .unwrap();
    let _ = SCP_ALLOW_ALL; // constant kept for readability in the module
}

#[tokio::test]
async fn no_organization_is_zero_behavior_change() {
    let server = start().await;
    let (akid, secret) = server.create_admin(ACCOUNT_A, "admin-a").await;
    let cfg = config_with(&server, &akid, &secret).await;
    let sqs = SqsClient::new(&cfg);
    // No CreateOrganization call — resolver returns None, evaluator
    // behaves exactly as before SCPs shipped.
    sqs.create_queue()
        .queue_name("pre-org-world")
        .send()
        .await
        .unwrap();
}
