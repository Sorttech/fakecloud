//! End-to-end tests for the minimal Organizations control plane
//! (Batch 1: CreateOrganization / DescribeOrganization / DeleteOrganization).
//!
//! Drives `aws-sdk-organizations` against a fakecloud server running in
//! `FAKECLOUD_IAM=strict` to prove the wire format matches and that the
//! service participates in multi-account dispatch correctly.

mod helpers;

use aws_credential_types::Credentials;
use aws_sdk_organizations::Client as OrgsClient;
use helpers::TestServer;

const ACCOUNT_A: &str = "111111111111";
const ACCOUNT_B: &str = "222222222222";

async fn start() -> TestServer {
    TestServer::start_with_env(&[
        ("FAKECLOUD_IAM", "strict"),
        ("FAKECLOUD_VERIFY_SIGV4", "true"),
        // Organizations is pure control plane; no container runtime
        // needed. Skip the reaper to keep CI fast and avoid flaky
        // docker-info probes on machines where the daemon is slow.
        ("FAKECLOUD_CONTAINER_CLI", "false"),
    ])
    .await
}

async fn config_with(server: &TestServer, akid: &str, secret: &str) -> aws_config::SdkConfig {
    aws_config::defaults(aws_config::BehaviorVersion::latest())
        .endpoint_url(server.endpoint())
        .region(aws_config::Region::new("us-east-1"))
        .credentials_provider(Credentials::new(akid, secret, None, None, "orgs-test"))
        .load()
        .await
}

#[tokio::test]
async fn create_and_describe_round_trip() {
    let server = start().await;
    let (akid, secret) = server.create_admin(ACCOUNT_A, "admin-a").await;
    let cfg = config_with(&server, &akid, &secret).await;
    let orgs = OrgsClient::new(&cfg);

    let created = orgs.create_organization().send().await.unwrap();
    let org = created.organization().unwrap();
    assert_eq!(org.master_account_id().unwrap(), ACCOUNT_A);
    assert_eq!(
        org.feature_set().unwrap(),
        &aws_sdk_organizations::types::OrganizationFeatureSet::All
    );
    assert!(org.id().unwrap().starts_with("o-"));

    let described = orgs.describe_organization().send().await.unwrap();
    let org2 = described.organization().unwrap();
    assert_eq!(org2.id(), org.id());
    assert_eq!(org2.master_account_id().unwrap(), ACCOUNT_A);
}

#[tokio::test]
async fn second_create_fails_with_already_in_org() {
    let server = start().await;
    let (akid, secret) = server.create_admin(ACCOUNT_A, "admin-a").await;
    let cfg = config_with(&server, &akid, &secret).await;
    let orgs = OrgsClient::new(&cfg);

    orgs.create_organization().send().await.unwrap();
    let err = orgs.create_organization().send().await.unwrap_err();
    let msg = format!("{err:?}");
    assert!(
        msg.contains("AlreadyInOrganizationException"),
        "expected AlreadyInOrganizationException, got: {msg}"
    );
}

#[tokio::test]
async fn describe_without_org_returns_not_in_use() {
    let server = start().await;
    let (akid, secret) = server.create_admin(ACCOUNT_A, "admin-a").await;
    let cfg = config_with(&server, &akid, &secret).await;
    let orgs = OrgsClient::new(&cfg);

    let err = orgs.describe_organization().send().await.unwrap_err();
    let msg = format!("{err:?}");
    assert!(
        msg.contains("AWSOrganizationsNotInUseException"),
        "expected AWSOrganizationsNotInUseException, got: {msg}"
    );
}

#[tokio::test]
async fn only_management_can_delete_organization() {
    let server = start().await;
    let (a_akid, a_secret) = server.create_admin(ACCOUNT_A, "admin-a").await;
    let (b_akid, b_secret) = server.create_admin(ACCOUNT_B, "admin-b").await;

    let a_cfg = config_with(&server, &a_akid, &a_secret).await;
    let b_cfg = config_with(&server, &b_akid, &b_secret).await;
    let orgs_a = OrgsClient::new(&a_cfg);
    let orgs_b = OrgsClient::new(&b_cfg);

    // Account A creates the organization -> A is the management account.
    orgs_a.create_organization().send().await.unwrap();

    // Account B is not a member of the organization, so both
    // DescribeOrganization and DeleteOrganization must look exactly
    // like "no org exists" — we don't leak org metadata to non-members.
    let err = orgs_b.describe_organization().send().await.unwrap_err();
    assert!(format!("{err:?}").contains("AWSOrganizationsNotInUseException"));
    let err = orgs_b.delete_organization().send().await.unwrap_err();
    let msg = format!("{err:?}");
    assert!(
        msg.contains("AWSOrganizationsNotInUseException"),
        "expected AWSOrganizationsNotInUseException, got: {msg}"
    );

    // Management account deletes successfully.
    orgs_a.delete_organization().send().await.unwrap();

    // Describe now fails again -> state really went back to None.
    let err = orgs_a.describe_organization().send().await.unwrap_err();
    assert!(format!("{err:?}").contains("AWSOrganizationsNotInUseException"));
}

#[tokio::test]
async fn list_roots_after_create() {
    let server = start().await;
    let (akid, secret) = server.create_admin(ACCOUNT_A, "admin-a").await;
    let cfg = config_with(&server, &akid, &secret).await;
    let orgs = OrgsClient::new(&cfg);

    orgs.create_organization().send().await.unwrap();
    let roots = orgs.list_roots().send().await.unwrap();
    let roots = roots.roots();
    assert_eq!(roots.len(), 1);
    assert!(roots[0].id().unwrap().starts_with("r-"));
}

#[tokio::test]
async fn ou_tree_crud_and_move_account() {
    let server = start().await;
    let (a_akid, a_secret) = server.create_admin(ACCOUNT_A, "admin-a").await;
    let a_cfg = config_with(&server, &a_akid, &a_secret).await;
    let orgs = OrgsClient::new(&a_cfg);

    let org_id = orgs
        .create_organization()
        .send()
        .await
        .unwrap()
        .organization()
        .unwrap()
        .id()
        .unwrap()
        .to_string();
    let root_id = orgs.list_roots().send().await.unwrap().roots()[0]
        .id()
        .unwrap()
        .to_string();

    // Bootstrap account B as a member of the org so it can be moved
    // between OUs. `create_admin` alone leaves an account standalone.
    let (_b_akid, _b_secret) = server
        .create_admin_in_org(ACCOUNT_B, "admin-b", &org_id)
        .await;

    let ou = orgs
        .create_organizational_unit()
        .parent_id(&root_id)
        .name("engineering")
        .send()
        .await
        .unwrap();
    let ou_id = ou.organizational_unit().unwrap().id().unwrap().to_string();

    // Duplicate name under same parent -> DuplicateOrganizationalUnitException.
    let err = orgs
        .create_organizational_unit()
        .parent_id(&root_id)
        .name("engineering")
        .send()
        .await
        .unwrap_err();
    assert!(format!("{err:?}").contains("DuplicateOrganizationalUnitException"));

    // Move account B from root to the new OU.
    orgs.move_account()
        .account_id(ACCOUNT_B)
        .source_parent_id(&root_id)
        .destination_parent_id(&ou_id)
        .send()
        .await
        .unwrap();

    let in_ou = orgs
        .list_accounts_for_parent()
        .parent_id(&ou_id)
        .send()
        .await
        .unwrap();
    assert_eq!(in_ou.accounts().len(), 1);
    assert_eq!(in_ou.accounts()[0].id().unwrap(), ACCOUNT_B);

    // Deleting the non-empty OU must fail.
    let err = orgs
        .delete_organizational_unit()
        .organizational_unit_id(&ou_id)
        .send()
        .await
        .unwrap_err();
    assert!(format!("{err:?}").contains("OrganizationalUnitNotEmptyException"));

    // Move back, then delete — should succeed.
    orgs.move_account()
        .account_id(ACCOUNT_B)
        .source_parent_id(&ou_id)
        .destination_parent_id(&root_id)
        .send()
        .await
        .unwrap();
    orgs.delete_organizational_unit()
        .organizational_unit_id(&ou_id)
        .send()
        .await
        .unwrap();
}

/// Regression for #2543: bootstrapping an admin while somebody else's
/// organization exists must leave the new account standalone. Silently
/// enrolling it handed the account another organization's SCP ceiling,
/// exposed that organization's metadata to it, and made it a stack-set
/// auto-deployment target.
/// See `two_accounts_each_run_their_own_organization` for the other
/// half of #2543: a second management account running an organization
/// of its own.
#[tokio::test]
async fn create_admin_leaves_the_account_outside_an_existing_organization() {
    let server = start().await;
    let (a_akid, a_secret) = server.create_admin(ACCOUNT_A, "admin-a").await;
    let a_cfg = config_with(&server, &a_akid, &a_secret).await;
    let orgs_a = OrgsClient::new(&a_cfg);
    orgs_a.create_organization().send().await.unwrap();

    // B is bootstrapped after A's org exists.
    let (b_akid, b_secret) = server.create_admin(ACCOUNT_B, "admin-b").await;
    let b_cfg = config_with(&server, &b_akid, &b_secret).await;
    let orgs_b = OrgsClient::new(&b_cfg);

    // B is a non-member: A's org is invisible to it.
    let err = orgs_b.describe_organization().send().await.unwrap_err();
    assert!(
        format!("{err:?}").contains("AWSOrganizationsNotInUseException"),
        "B must not be enrolled into A's organization, got: {err:?}"
    );

    // ...and A's organization has exactly one account, the management one.
    let accounts = orgs_a.list_accounts().send().await.unwrap();
    let ids: Vec<&str> = accounts.accounts().iter().filter_map(|a| a.id()).collect();
    assert_eq!(ids, [ACCOUNT_A]);
}

/// The whole of #2543's repro: two management accounts, each with its
/// own organization and its own child accounts, fully independent.
/// Before multi-organization support the second `CreateOrganization`
/// failed with `AlreadyInOrganizationException` purely because the
/// first organization existed.
#[tokio::test]
async fn two_accounts_each_run_their_own_organization() {
    let server = start().await;

    // Distinct from the management ids: reusing an owner's own id as a
    // child would re-bootstrap that account's admin user and invalidate
    // the credentials this test is already holding.
    const CHILDREN_A: [&str; 2] = ["111111110001", "111111110002"];
    const CHILDREN_B: [&str; 2] = ["222222220001", "222222220002"];

    let mut orgs_by_owner = Vec::new();
    for (owner, children) in [(ACCOUNT_A, CHILDREN_A), (ACCOUNT_B, CHILDREN_B)] {
        let (akid, secret) = server.create_admin(owner, "root").await;
        let cfg = config_with(&server, &akid, &secret).await;
        let orgs = OrgsClient::new(&cfg);

        let org_id = orgs
            .create_organization()
            .feature_set(aws_sdk_organizations::types::OrganizationFeatureSet::All)
            .send()
            .await
            .unwrap()
            .organization()
            .unwrap()
            .id()
            .unwrap()
            .to_string();

        // Two children per organization, enrolled at bootstrap.
        for child in children {
            server.create_admin_in_org(child, "root", &org_id).await;
        }
        orgs_by_owner.push((owner, orgs, org_id, children));
    }

    let (_, orgs_a, org_a, children_a) = &orgs_by_owner[0];
    let (_, orgs_b, org_b, children_b) = &orgs_by_owner[1];
    assert_ne!(org_a, org_b, "each account gets its own organization");

    // Each management account sees only its own organization...
    assert_eq!(
        orgs_a
            .describe_organization()
            .send()
            .await
            .unwrap()
            .organization()
            .unwrap()
            .id()
            .unwrap(),
        org_a
    );
    assert_eq!(
        orgs_b
            .describe_organization()
            .send()
            .await
            .unwrap()
            .organization()
            .unwrap()
            .id()
            .unwrap(),
        org_b
    );

    // ...and only its own accounts: management plus its two children,
    // with nothing from the other organization leaking in.
    for (owner, orgs, _, children) in [
        (ACCOUNT_A, orgs_a, org_a, children_a),
        (ACCOUNT_B, orgs_b, org_b, children_b),
    ] {
        let listed = orgs.list_accounts().send().await.unwrap();
        let mut ids: Vec<String> = listed
            .accounts()
            .iter()
            .filter_map(|a| a.id())
            .map(str::to_string)
            .collect();
        ids.sort();
        let mut expected: Vec<String> = children.iter().map(|c| c.to_string()).collect();
        expected.push(owner.to_string());
        expected.sort();
        assert_eq!(
            ids, expected,
            "organization of {owner} has the wrong members"
        );
    }
}

/// An account can only ever be in one organization: an invitation to an
/// account another organization already holds is rejected.
#[tokio::test]
async fn an_account_cannot_be_invited_into_a_second_organization() {
    let server = start().await;
    let (a_akid, a_secret) = server.create_admin(ACCOUNT_A, "admin-a").await;
    let a_cfg = config_with(&server, &a_akid, &a_secret).await;
    let orgs_a = OrgsClient::new(&a_cfg);
    orgs_a.create_organization().send().await.unwrap();

    let (b_akid, b_secret) = server.create_admin(ACCOUNT_B, "admin-b").await;
    let b_cfg = config_with(&server, &b_akid, &b_secret).await;
    let orgs_b = OrgsClient::new(&b_cfg);
    orgs_b.create_organization().send().await.unwrap();

    let err = orgs_a
        .invite_account_to_organization()
        .target(
            aws_sdk_organizations::types::HandshakeParty::builder()
                .id(ACCOUNT_B)
                .r#type(aws_sdk_organizations::types::HandshakePartyType::Account)
                .build()
                .unwrap(),
        )
        .send()
        .await
        .unwrap_err();
    assert!(
        format!("{err:?}").contains("HandshakeConstraintViolationException"),
        "expected HandshakeConstraintViolationException, got: {err:?}"
    );
}

/// The opt-in half of #2543: naming an organization on the bootstrap
/// request enrolls the account into it, the shortcut equivalent of an
/// invite/accept handshake.
#[tokio::test]
async fn create_admin_with_organization_id_enrolls_the_account() {
    let server = start().await;
    let (a_akid, a_secret) = server.create_admin(ACCOUNT_A, "admin-a").await;
    let a_cfg = config_with(&server, &a_akid, &a_secret).await;
    let orgs_a = OrgsClient::new(&a_cfg);
    let org_id = orgs_a
        .create_organization()
        .send()
        .await
        .unwrap()
        .organization()
        .unwrap()
        .id()
        .unwrap()
        .to_string();

    let (b_akid, b_secret) = server
        .create_admin_in_org(ACCOUNT_B, "admin-b", &org_id)
        .await;
    let b_cfg = config_with(&server, &b_akid, &b_secret).await;
    let orgs_b = OrgsClient::new(&b_cfg);

    // B now sees the org it belongs to.
    let described = orgs_b.describe_organization().send().await.unwrap();
    assert_eq!(described.organization().unwrap().id().unwrap(), org_id);

    let accounts = orgs_a.list_accounts().send().await.unwrap();
    let mut ids: Vec<&str> = accounts.accounts().iter().filter_map(|a| a.id()).collect();
    ids.sort_unstable();
    assert_eq!(ids, [ACCOUNT_A, ACCOUNT_B]);
}

#[tokio::test]
async fn non_management_member_cannot_create_ou() {
    let server = start().await;
    let (a_akid, a_secret) = server.create_admin(ACCOUNT_A, "admin-a").await;
    let a_cfg = config_with(&server, &a_akid, &a_secret).await;
    let orgs_a = OrgsClient::new(&a_cfg);

    // B must be enrolled as a member — a non-member caller gets
    // `AWSOrganizationsNotInUseException` instead of the
    // `AccessDeniedException` this test is about.
    let org_id = orgs_a
        .create_organization()
        .send()
        .await
        .unwrap()
        .organization()
        .unwrap()
        .id()
        .unwrap()
        .to_string();
    let root_id = orgs_a.list_roots().send().await.unwrap().roots()[0]
        .id()
        .unwrap()
        .to_string();

    let (b_akid, b_secret) = server
        .create_admin_in_org(ACCOUNT_B, "admin-b", &org_id)
        .await;
    let b_cfg = config_with(&server, &b_akid, &b_secret).await;
    let orgs_b = OrgsClient::new(&b_cfg);

    let err = orgs_b
        .create_organizational_unit()
        .parent_id(&root_id)
        .name("forbidden")
        .send()
        .await
        .unwrap_err();
    assert!(format!("{err:?}").contains("AccessDeniedException"));
}

const SCP_ALLOW_ALL: &str =
    r#"{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Action":"*","Resource":"*"}]}"#;

#[tokio::test]
async fn scp_create_attach_detach_delete_roundtrip() {
    let server = start().await;
    let (akid, secret) = server.create_admin(ACCOUNT_A, "admin-a").await;
    let cfg = config_with(&server, &akid, &secret).await;
    let orgs = OrgsClient::new(&cfg);

    orgs.create_organization().send().await.unwrap();
    let root_id = orgs.list_roots().send().await.unwrap().roots()[0]
        .id()
        .unwrap()
        .to_string();

    // CreatePolicy -> returned id starts with p-.
    let created = orgs
        .create_policy()
        .name("CustomGuardrail")
        .description("test SCP")
        .r#type(aws_sdk_organizations::types::PolicyType::ServiceControlPolicy)
        .content(SCP_ALLOW_ALL)
        .send()
        .await
        .unwrap();
    let policy_id = created
        .policy()
        .unwrap()
        .policy_summary()
        .unwrap()
        .id()
        .unwrap()
        .to_string();
    assert!(policy_id.starts_with("p-"));

    // Attach to root.
    orgs.attach_policy()
        .policy_id(&policy_id)
        .target_id(&root_id)
        .send()
        .await
        .unwrap();

    // Delete attached -> fails.
    let err = orgs
        .delete_policy()
        .policy_id(&policy_id)
        .send()
        .await
        .unwrap_err();
    assert!(format!("{err:?}").contains("PolicyInUseException"));

    // ListPoliciesForTarget sees Custom + FullAWSAccess.
    let list = orgs
        .list_policies_for_target()
        .target_id(&root_id)
        .filter(aws_sdk_organizations::types::PolicyType::ServiceControlPolicy)
        .send()
        .await
        .unwrap();
    let names: Vec<String> = list
        .policies()
        .iter()
        .map(|p| p.name().unwrap().to_string())
        .collect();
    assert!(names.contains(&"CustomGuardrail".to_string()));
    assert!(names.contains(&"FullAWSAccess".to_string()));

    // ListTargetsForPolicy sees the root.
    let targets = orgs
        .list_targets_for_policy()
        .policy_id(&policy_id)
        .send()
        .await
        .unwrap();
    assert_eq!(targets.targets().len(), 1);
    assert_eq!(targets.targets()[0].target_id().unwrap(), root_id);

    // Detach + delete succeed.
    orgs.detach_policy()
        .policy_id(&policy_id)
        .target_id(&root_id)
        .send()
        .await
        .unwrap();
    orgs.delete_policy()
        .policy_id(&policy_id)
        .send()
        .await
        .unwrap();
}

#[tokio::test]
async fn scp_full_aws_access_is_immutable() {
    let server = start().await;
    let (akid, secret) = server.create_admin(ACCOUNT_A, "admin-a").await;
    let cfg = config_with(&server, &akid, &secret).await;
    let orgs = OrgsClient::new(&cfg);

    orgs.create_organization().send().await.unwrap();
    let err = orgs
        .delete_policy()
        .policy_id("p-FullAWSAccess")
        .send()
        .await
        .unwrap_err();
    assert!(format!("{err:?}").contains("PolicyChangesNotAllowedException"));
}
