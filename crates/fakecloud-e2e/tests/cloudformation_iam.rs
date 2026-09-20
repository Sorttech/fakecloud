//! CloudFormation provisioner for IAM User/Group/ManagedPolicy/AccessKey/InstanceProfile.

mod helpers;

use aws_sdk_cloudformation::types::{Capability, OnFailure};
use helpers::TestServer;

const TEMPLATE: &str = r#"{
  "AWSTemplateFormatVersion": "2010-09-09",
  "Resources": {
    "Devs": {
      "Type": "AWS::IAM::Group",
      "Properties": {"GroupName": "cfn-devs"}
    },
    "Alice": {
      "Type": "AWS::IAM::User",
      "Properties": {
        "UserName": "cfn-alice",
        "Tags": [{"Key": "team", "Value": "platform"}]
      }
    },
    "AliceKey": {
      "Type": "AWS::IAM::AccessKey",
      "Properties": {"UserName": {"Ref": "Alice"}}
    },
    "Membership": {
      "Type": "AWS::IAM::UserToGroupAddition",
      "Properties": {
        "GroupName": {"Ref": "Devs"},
        "Users": [{"Ref": "Alice"}]
      }
    },
    "ReadOnly": {
      "Type": "AWS::IAM::ManagedPolicy",
      "Properties": {
        "ManagedPolicyName": "cfn-readonly",
        "Description": "read-only policy",
        "PolicyDocument": {
          "Version": "2012-10-17",
          "Statement": [{"Effect": "Allow", "Action": "s3:GetObject", "Resource": "*"}]
        },
        "Users": [{"Ref": "Alice"}]
      }
    },
    "ServerRole": {
      "Type": "AWS::IAM::Role",
      "Properties": {
        "RoleName": "cfn-ec2-role",
        "AssumeRolePolicyDocument": {
          "Version": "2012-10-17",
          "Statement": [{"Effect": "Allow", "Principal": {"Service": "ec2.amazonaws.com"}, "Action": "sts:AssumeRole"}]
        }
      }
    },
    "ServerProfile": {
      "Type": "AWS::IAM::InstanceProfile",
      "Properties": {
        "InstanceProfileName": "cfn-ec2-profile",
        "Roles": [{"Ref": "ServerRole"}]
      }
    }
  },
  "Outputs": {
    "AliceArn": {"Value": {"Fn::GetAtt": ["Alice", "Arn"]}},
    "ProfileArn": {"Value": {"Fn::GetAtt": ["ServerProfile", "Arn"]}},
    "PolicyArn": {"Value": {"Ref": "ReadOnly"}}
  }
}"#;

#[tokio::test]
async fn cfn_provisions_iam_user_group_policy_etc() {
    let server = TestServer::start().await;
    let cfn = server.cloudformation_client().await;
    let iam = server.iam_client().await;

    cfn.create_stack()
        .stack_name("iam-stack")
        .template_body(TEMPLATE)
        .capabilities(Capability::CapabilityNamedIam)
        .on_failure(OnFailure::Rollback)
        .send()
        .await
        .expect("create_stack");

    let described = cfn
        .describe_stacks()
        .stack_name("iam-stack")
        .send()
        .await
        .expect("describe_stacks");
    let stack = described.stacks().first().expect("stack present");
    assert_eq!(stack.stack_status().unwrap().as_str(), "CREATE_COMPLETE");

    let user = iam
        .get_user()
        .user_name("cfn-alice")
        .send()
        .await
        .expect("get_user");
    let u = user.user().expect("user");
    assert_eq!(u.user_name(), "cfn-alice");

    let groups = iam
        .list_groups_for_user()
        .user_name("cfn-alice")
        .send()
        .await
        .expect("list_groups_for_user");
    assert!(
        groups.groups().iter().any(|g| g.group_name() == "cfn-devs"),
        "alice should be in cfn-devs"
    );

    let attached = iam
        .list_attached_user_policies()
        .user_name("cfn-alice")
        .send()
        .await
        .expect("list_attached_user_policies");
    assert!(
        attached
            .attached_policies()
            .iter()
            .any(|p| p.policy_name() == Some("cfn-readonly")),
        "managed policy should be attached to alice"
    );

    let keys = iam
        .list_access_keys()
        .user_name("cfn-alice")
        .send()
        .await
        .expect("list_access_keys");
    assert_eq!(keys.access_key_metadata().len(), 1);

    let profile = iam
        .get_instance_profile()
        .instance_profile_name("cfn-ec2-profile")
        .send()
        .await
        .expect("get_instance_profile");
    let p = profile.instance_profile().expect("profile");
    assert_eq!(p.instance_profile_name(), "cfn-ec2-profile");
    assert_eq!(p.roles().len(), 1);

    cfn.delete_stack()
        .stack_name("iam-stack")
        .send()
        .await
        .expect("delete_stack");

    let after_user = iam.get_user().user_name("cfn-alice").send().await;
    assert!(after_user.is_err(), "user should be gone after delete");
}

#[tokio::test]
async fn cfn_instance_profile_id_matches_the_one_ec2_renders() {
    // The CloudFormation provisioner is a third constructor of an instance
    // profile, alongside IAM's CreateInstanceProfile and EC2's association
    // render. It used to mint a random 20-character id, so a CFN-created
    // profile attached to an instance reported one id through GetInstanceProfile
    // and a different one through DescribeInstances.
    let server = TestServer::start().await;
    let cfn = server.cloudformation_client().await;
    let iam = server.iam_client().await;
    let ec2 = server.ec2_client().await;

    cfn.create_stack()
        .stack_name("profile-id-stack")
        .template_body(TEMPLATE)
        .capabilities(Capability::CapabilityNamedIam)
        .on_failure(OnFailure::Rollback)
        .send()
        .await
        .expect("create_stack");

    let profile = iam
        .get_instance_profile()
        .instance_profile_name("cfn-ec2-profile")
        .send()
        .await
        .expect("get_instance_profile");
    let profile = profile.instance_profile().expect("instance profile");
    let iam_id = profile.instance_profile_id().to_string();
    assert_eq!(iam_id.len(), 21, "AWS ids are 21 characters: {iam_id}");

    let instance = ec2
        .run_instances()
        .image_id("ami-0123456789abcdef0")
        .min_count(1)
        .max_count(1)
        .send()
        .await
        .expect("run_instances");
    let instance_id = instance.instances()[0]
        .instance_id()
        .expect("instance id")
        .to_string();
    ec2.associate_iam_instance_profile()
        .instance_id(&instance_id)
        .iam_instance_profile(
            aws_sdk_ec2::types::IamInstanceProfileSpecification::builder()
                .arn(profile.arn())
                .build(),
        )
        .send()
        .await
        .expect("associate_iam_instance_profile");

    let desc = ec2
        .describe_instances()
        .instance_ids(&instance_id)
        .send()
        .await
        .expect("describe_instances");
    let rendered = desc.reservations()[0].instances()[0]
        .iam_instance_profile()
        .expect("profile on instance");
    assert_eq!(rendered.id(), Some(iam_id.as_str()));
}
