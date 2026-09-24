//! End-to-end coverage for StackSets auto-deployment: a service-managed stack
//! set with `AutoDeployment.Enabled` follows the organization. An account that
//! joins a target OU after the stack instances were created gets the stacks
//! too, and an account that leaves loses them.

mod helpers;

use aws_credential_types::Credentials;
use aws_sdk_cloudformation::error::ProvideErrorMetadata;
use aws_sdk_cloudformation::types::{
    AutoDeployment, Capability, DeploymentTargets, PermissionModels, StackSetOperationStatus,
};
use helpers::TestServer;

/// The account the default test credentials resolve to; it owns the org.
const MANAGEMENT: &str = "123456789012";
const CHILD_ONE: &str = "111111111111";
/// Joins the organization only after the stack instances exist.
const CHILD_TWO: &str = "222222222222";

const STACKSETS_PRINCIPAL: &str = "member.org.stacksets.cloudformation.amazonaws.com";
const ROLE_NAME: &str = "readonly-role";

const TEMPLATE: &str = r#"{
    "Resources": {
        "ReadOnlyExecutionRole": {
            "Type": "AWS::IAM::Role",
            "Properties": {
                "RoleName": "readonly-role",
                "AssumeRolePolicyDocument": {
                    "Version": "2012-10-17",
                    "Statement": [{
                        "Effect": "Allow",
                        "Principal": { "AWS": "arn:aws:iam::123456789012:root" },
                        "Action": "sts:AssumeRole"
                    }]
                },
                "Path": "/"
            }
        }
    }
}"#;

async fn start() -> TestServer {
    TestServer::start_with_env(&[
        // Organizations + IAM + StackSets are pure control plane here.
        ("FAKECLOUD_CONTAINER_CLI", "false"),
    ])
    .await
}

/// An IAM client whose credentials belong to `account_id`, bootstrapped
/// as a member of `org_id` — joining the organization is what makes the
/// account an auto-deployment target.
async fn iam_for(server: &TestServer, account_id: &str, org_id: &str) -> aws_sdk_iam::Client {
    let (akid, secret) = server.create_admin_in_org(account_id, "root", org_id).await;
    let config = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .endpoint_url(server.endpoint())
        .region(aws_config::Region::new("us-east-1"))
        .credentials_provider(Credentials::new(akid, secret, None, None, "member"))
        .load()
        .await;
    aws_sdk_iam::Client::new(&config)
}

async fn has_role(iam: &aws_sdk_iam::Client) -> bool {
    match iam.get_role().role_name(ROLE_NAME).send().await {
        Ok(_) => true,
        Err(e) => {
            assert_eq!(
                e.code(),
                Some("NoSuchEntity"),
                "unexpected GetRole error: {e:?}"
            );
            false
        }
    }
}

async fn operation_status(
    cfn: &aws_sdk_cloudformation::Client,
    stack_set: &str,
    operation_id: &str,
) -> StackSetOperationStatus {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        let status = cfn
            .describe_stack_set_operation()
            .stack_set_name(stack_set)
            .operation_id(operation_id)
            .send()
            .await
            .unwrap()
            .stack_set_operation()
            .and_then(|op| op.status())
            .cloned()
            .expect("operation status");
        if !matches!(
            status,
            StackSetOperationStatus::Running
                | StackSetOperationStatus::Queued
                | StackSetOperationStatus::Stopping
        ) {
            return status;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "operation {operation_id} never finished"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

async fn instance_accounts(cfn: &aws_sdk_cloudformation::Client, stack_set: &str) -> Vec<String> {
    let mut accounts: Vec<String> = cfn
        .list_stack_instances()
        .stack_set_name(stack_set)
        .send()
        .await
        .unwrap()
        .summaries()
        .iter()
        .filter_map(|s| s.account().map(str::to_string))
        .collect();
    accounts.sort();
    accounts
}

#[tokio::test]
async fn auto_deployment_follows_accounts_in_and_out_of_the_target_ou() {
    let server = start().await;
    let orgs = server.organizations_client().await;
    let cfn = server.cloudformation_client().await;

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
    let root = orgs.list_roots().send().await.unwrap().roots()[0]
        .id()
        .unwrap()
        .to_string();
    orgs.enable_aws_service_access()
        .service_principal(STACKSETS_PRINCIPAL)
        .send()
        .await
        .unwrap();

    // One member account exists before the stack set is deployed.
    let child_one = iam_for(&server, CHILD_ONE, &org_id).await;

    cfn.create_stack_set()
        .stack_set_name("auto")
        .template_body(TEMPLATE)
        .permission_model(PermissionModels::ServiceManaged)
        .capabilities(Capability::CapabilityNamedIam)
        .auto_deployment(
            AutoDeployment::builder()
                .enabled(true)
                .retain_stacks_on_account_removal(false)
                .build(),
        )
        .send()
        .await
        .unwrap();
    let create = cfn
        .create_stack_instances()
        .stack_set_name("auto")
        .deployment_targets(
            DeploymentTargets::builder()
                .organizational_unit_ids(&root)
                .build(),
        )
        .regions("us-east-1")
        .send()
        .await
        .unwrap();
    assert_eq!(
        operation_status(&cfn, "auto", create.operation_id().unwrap()).await,
        StackSetOperationStatus::Succeeded
    );
    assert!(has_role(&child_one).await, "member at deploy time");

    // The management account is never a service-managed target.
    assert_eq!(
        orgs.describe_organization()
            .send()
            .await
            .unwrap()
            .organization()
            .and_then(|o| o.master_account_id()),
        Some(MANAGEMENT)
    );
    let management = server.iam_client().await;
    assert!(!has_role(&management).await, "management account");
    assert_eq!(instance_accounts(&cfn, "auto").await, [CHILD_ONE]);

    // The reported bug: an account that joins the target OU afterwards was
    // left without the stack set's stacks.
    let child_two = iam_for(&server, CHILD_TWO, &org_id).await;
    assert!(has_role(&child_two).await, "account added after deployment");
    assert_eq!(
        instance_accounts(&cfn, "auto").await,
        [CHILD_ONE, CHILD_TWO]
    );

    // Auto-deployment is recorded as an ordinary CREATE operation on the
    // stack set, against the OU that gained the account.
    let ops = cfn
        .list_stack_set_operations()
        .stack_set_name("auto")
        .send()
        .await
        .unwrap();
    assert_eq!(ops.summaries().len(), 2, "{ops:?}");
    let targets = cfn
        .list_stack_set_auto_deployment_targets()
        .stack_set_name("auto")
        .send()
        .await
        .unwrap();
    assert_eq!(targets.summaries().len(), 1);
    assert_eq!(
        targets.summaries()[0].organizational_unit_id(),
        Some(&root[..])
    );

    // Leaving the organization takes the stacks with it
    // (RetainStacksOnAccountRemoval=false).
    orgs.remove_account_from_organization()
        .account_id(CHILD_TWO)
        .send()
        .await
        .unwrap();
    assert_eq!(instance_accounts(&cfn, "auto").await, [CHILD_ONE]);
    assert!(
        !has_role(&child_two).await,
        "stack removed with the account"
    );
    assert!(has_role(&child_one).await, "untouched member");
}

#[tokio::test]
async fn auto_deployment_can_retain_stacks_when_an_account_is_removed() {
    let server = start().await;
    let orgs = server.organizations_client().await;
    let cfn = server.cloudformation_client().await;

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
    let root = orgs.list_roots().send().await.unwrap().roots()[0]
        .id()
        .unwrap()
        .to_string();
    orgs.enable_aws_service_access()
        .service_principal(STACKSETS_PRINCIPAL)
        .send()
        .await
        .unwrap();
    let child_one = iam_for(&server, CHILD_ONE, &org_id).await;

    cfn.create_stack_set()
        .stack_set_name("retain")
        .template_body(TEMPLATE)
        .permission_model(PermissionModels::ServiceManaged)
        .capabilities(Capability::CapabilityNamedIam)
        .auto_deployment(
            AutoDeployment::builder()
                .enabled(true)
                .retain_stacks_on_account_removal(true)
                .build(),
        )
        .send()
        .await
        .unwrap();
    let create = cfn
        .create_stack_instances()
        .stack_set_name("retain")
        .deployment_targets(
            DeploymentTargets::builder()
                .organizational_unit_ids(&root)
                .build(),
        )
        .regions("us-east-1")
        .send()
        .await
        .unwrap();
    assert_eq!(
        operation_status(&cfn, "retain", create.operation_id().unwrap()).await,
        StackSetOperationStatus::Succeeded
    );
    assert!(has_role(&child_one).await);

    orgs.remove_account_from_organization()
        .account_id(CHILD_ONE)
        .send()
        .await
        .unwrap();
    assert!(instance_accounts(&cfn, "retain").await.is_empty());
    // The instance is gone from the stack set, but its stack stays behind.
    assert!(has_role(&child_one).await, "retained stack");

    // The OU is still the stack set's target even with nothing deployed in
    // it, so the next account to join is deployed to.
    let child_two = iam_for(&server, CHILD_TWO, &org_id).await;
    assert_eq!(instance_accounts(&cfn, "retain").await, [CHILD_TWO]);
    assert!(has_role(&child_two).await);
}

/// An `AWS::Organizations::Account` resource puts an account into the OU from
/// inside a CloudFormation stack, which never goes through the Organizations
/// API. Auto-deployment has to see that too.
#[tokio::test]
async fn auto_deployment_covers_an_account_created_by_a_cloudformation_stack() {
    let server = start().await;
    let orgs = server.organizations_client().await;
    let cfn = server.cloudformation_client().await;

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
    let root = orgs.list_roots().send().await.unwrap().roots()[0]
        .id()
        .unwrap()
        .to_string();
    orgs.enable_aws_service_access()
        .service_principal(STACKSETS_PRINCIPAL)
        .send()
        .await
        .unwrap();
    iam_for(&server, CHILD_ONE, &org_id).await;

    cfn.create_stack_set()
        .stack_set_name("auto")
        .template_body(TEMPLATE)
        .permission_model(PermissionModels::ServiceManaged)
        .capabilities(Capability::CapabilityNamedIam)
        .auto_deployment(AutoDeployment::builder().enabled(true).build())
        .send()
        .await
        .unwrap();
    let create = cfn
        .create_stack_instances()
        .stack_set_name("auto")
        .deployment_targets(
            DeploymentTargets::builder()
                .organizational_unit_ids(&root)
                .build(),
        )
        .regions("us-east-1")
        .send()
        .await
        .unwrap();
    assert_eq!(
        operation_status(&cfn, "auto", create.operation_id().unwrap()).await,
        StackSetOperationStatus::Succeeded
    );

    let account_template = format!(
        r#"{{"Resources":{{"Member":{{"Type":"AWS::Organizations::Account","Properties":{{"AccountName":"spawned","Email":"spawned@example.com","ParentIds":["{root}"]}}}}}}}}"#
    );
    cfn.create_stack()
        .stack_name("member-account")
        .template_body(account_template)
        .send()
        .await
        .unwrap();
    let account_id = helpers::wait_until(std::time::Duration::from_secs(30), || async {
        let accounts = orgs.list_accounts().send().await.unwrap();
        accounts
            .accounts()
            .iter()
            .find(|a| a.name() == Some("spawned"))
            .and_then(|a| a.id().map(str::to_string))
    })
    .await
    .expect("account provisioned by the stack");

    let deployed = helpers::wait_until(std::time::Duration::from_secs(30), || async {
        instance_accounts(&cfn, "auto")
            .await
            .contains(&account_id)
            .then_some(())
    })
    .await;
    assert!(
        deployed.is_some(),
        "stack-created account never got the stack set: {:?}",
        instance_accounts(&cfn, "auto").await
    );
}
