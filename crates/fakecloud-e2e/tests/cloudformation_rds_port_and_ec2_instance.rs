//! CloudFormation provisioner fixes:
//! - An RDS DBInstance with no Port defaults the port from the engine
//!   (MySQL -> 3306) via Fn::GetAtt Endpoint.Port, not a hardcoded 5432.
//! - Fn::GetAtt DbiResourceId matches the value DescribeDBInstances returns
//!   (previously the GetAtt refabricated `db-<identifier>` while the record
//!   stored a real `db-<uuid>`).
//! - AWS::EC2::Instance provisions a REAL instance: Ref resolves to the
//!   `i-...` id and DescribeInstances finds it (previously it fell to the
//!   accept-and-ignore catch-all and Ref returned the bare logical id).

mod helpers;

use aws_sdk_cloudformation::types::Capability;
use helpers::TestServer;

const TEMPLATE: &str = r#"{
  "AWSTemplateFormatVersion": "2010-09-09",
  "Resources": {
    "Db": {
      "Type": "AWS::RDS::DBInstance",
      "Properties": {
        "DBInstanceIdentifier": "mysql-noport",
        "DBInstanceClass": "db.t4g.micro",
        "Engine": "mysql",
        "EngineVersion": "8.0",
        "MasterUsername": "admin",
        "MasterUserPassword": "hunter2-secret",
        "AllocatedStorage": "20"
      }
    },
    "Instance": {
      "Type": "AWS::EC2::Instance",
      "Properties": {
        "ImageId": "ami-0123456789abcdef0",
        "InstanceType": "t3.micro"
      }
    }
  },
  "Outputs": {
    "DbPort": {"Value": {"Fn::GetAtt": ["Db", "Endpoint.Port"]}},
    "DbResourceId": {"Value": {"Fn::GetAtt": ["Db", "DbiResourceId"]}},
    "InstanceRef": {"Value": {"Ref": "Instance"}},
    "InstanceAz": {"Value": {"Fn::GetAtt": ["Instance", "AvailabilityZone"]}}
  }
}"#;

fn output<'a>(stack: &'a aws_sdk_cloudformation::types::Stack, key: &str) -> &'a str {
    stack
        .outputs()
        .iter()
        .find(|o| o.output_key() == Some(key))
        .and_then(|o| o.output_value())
        .unwrap_or_else(|| panic!("output {key} present"))
}

#[tokio::test]
async fn cfn_rds_engine_port_and_real_ec2_instance() {
    let server = TestServer::start().await;
    let cfn = server.cloudformation_client().await;
    let rds = server.rds_client().await;
    let ec2 = server.ec2_client().await;

    cfn.create_stack()
        .stack_name("port-ec2-stack")
        .template_body(TEMPLATE)
        .capabilities(Capability::CapabilityIam)
        .send()
        .await
        .expect("create_stack");

    // CreateStack provisions a container-backed DB instance in the
    // background, as AWS does, so wait for the stack to settle.
    let stack = &helpers::wait_until(std::time::Duration::from_secs(300), || async {
        let out = cfn
            .describe_stacks()
            .stack_name("port-ec2-stack")
            .send()
            .await
            .ok()?;
        let s = out.stacks().first()?.clone();
        (!s.stack_status()?.as_str().ends_with("IN_PROGRESS")).then_some(s)
    })
    .await
    .expect("stack reached a terminal status");
    assert_eq!(stack.stack_status().unwrap().as_str(), "CREATE_COMPLETE");

    // DbiResourceId GetAtt matches DescribeDBInstances.
    let getatt_resource_id = output(stack, "DbResourceId");
    let inst = rds
        .describe_db_instances()
        .db_instance_identifier("mysql-noport")
        .send()
        .await
        .expect("describe_db_instances");
    let db = inst.db_instances().first().expect("db present");
    assert_eq!(db.dbi_resource_id(), Some(getatt_resource_id));
    assert!(
        getatt_resource_id.starts_with("db-"),
        "resource id should be db-<uuid>: {getatt_resource_id}"
    );
    // The Endpoint.Port output is the port DescribeDBInstances reports: the
    // MySQL default (3306, not 5432) without a container runtime, the
    // container's published port with one. The default itself is pinned by
    // the provisioner unit test `rds_mysql_instance_without_port_uses_engine_default`.
    let port = db.endpoint().and_then(|e| e.port()).expect("port");
    assert_eq!(output(stack, "DbPort"), port.to_string());

    // EC2::Instance Ref resolves to a real i- id and DescribeInstances finds it.
    let instance_ref = output(stack, "InstanceRef");
    assert!(
        instance_ref.starts_with("i-"),
        "Ref should resolve to an instance id: {instance_ref}"
    );
    assert!(!output(stack, "InstanceAz").is_empty(), "AZ GetAtt present");

    let di = ec2
        .describe_instances()
        .instance_ids(instance_ref)
        .send()
        .await
        .expect("describe_instances");
    let found = di
        .reservations()
        .iter()
        .flat_map(|r| r.instances())
        .any(|i| i.instance_id() == Some(instance_ref));
    assert!(
        found,
        "CFN-created instance {instance_ref} must be described"
    );
}

const PROFILE_TEMPLATE: &str = r#"{
  "AWSTemplateFormatVersion": "2010-09-09",
  "Resources": {
    "Instance": {
      "Type": "AWS::EC2::Instance",
      "Properties": {
        "ImageId": "ami-0123456789abcdef0",
        "InstanceType": "t3.micro",
        "IamInstanceProfile": "PROFILE"
      }
    }
  },
  "Outputs": {
    "InstanceRef": {"Value": {"Ref": "Instance"}}
  }
}"#;

async fn instance_association_id(ec2: &aws_sdk_ec2::Client, id: &str) -> Option<String> {
    ec2.describe_iam_instance_profile_associations()
        .filters(
            aws_sdk_ec2::types::Filter::builder()
                .name("instance-id")
                .values(id)
                .build(),
        )
        .send()
        .await
        .expect("describe_iam_instance_profile_associations")
        .iam_instance_profile_associations()
        .first()
        .and_then(|a| a.association_id())
        .map(str::to_string)
}

async fn instance_profile_arn(ec2: &aws_sdk_ec2::Client, id: &str) -> Option<String> {
    ec2.describe_instances()
        .instance_ids(id)
        .send()
        .await
        .expect("describe_instances")
        .reservations()
        .iter()
        .flat_map(|r| r.instances())
        .find(|i| i.instance_id() == Some(id))
        .and_then(|i| i.iam_instance_profile())
        .and_then(|p| p.arn())
        .map(str::to_string)
}

#[tokio::test]
async fn cfn_update_swaps_the_ec2_instance_profile_in_place() {
    // An UpdateStack that changes IamInstanceProfile reported UPDATE_COMPLETE
    // while the instance kept the old profile, because the in-place update arm
    // never touched the association.
    let server = TestServer::start().await;
    let cfn = server.cloudformation_client().await;
    let ec2 = server.ec2_client().await;

    cfn.create_stack()
        .stack_name("profile-update-stack")
        .template_body(PROFILE_TEMPLATE.replace("PROFILE", "web-profile"))
        .capabilities(Capability::CapabilityIam)
        .send()
        .await
        .expect("create_stack");

    let described = cfn
        .describe_stacks()
        .stack_name("profile-update-stack")
        .send()
        .await
        .expect("describe_stacks");
    let stack = described.stacks().first().expect("stack present");
    assert_eq!(stack.stack_status().unwrap().as_str(), "CREATE_COMPLETE");
    let id = output(stack, "InstanceRef").to_string();
    assert_eq!(
        instance_profile_arn(&ec2, &id).await.as_deref(),
        Some("arn:aws:iam::123456789012:instance-profile/web-profile")
    );

    let before = instance_association_id(&ec2, &id)
        .await
        .expect("association");

    // An update that leaves IamInstanceProfile alone must not retire the
    // association: AWS only touches it when the profile itself changes.
    cfn.update_stack()
        .stack_name("profile-update-stack")
        .template_body(
            PROFILE_TEMPLATE
                .replace("PROFILE", "web-profile")
                .replace("t3.micro", "t3.small"),
        )
        .capabilities(Capability::CapabilityIam)
        .send()
        .await
        .expect("update_stack");
    assert_eq!(
        instance_association_id(&ec2, &id).await.as_deref(),
        Some(&*before)
    );

    cfn.update_stack()
        .stack_name("profile-update-stack")
        .template_body(PROFILE_TEMPLATE.replace("PROFILE", "admin-profile"))
        .capabilities(Capability::CapabilityIam)
        .send()
        .await
        .expect("update_stack");

    let described = cfn
        .describe_stacks()
        .stack_name("profile-update-stack")
        .send()
        .await
        .expect("describe_stacks");
    let stack = described.stacks().first().expect("stack present");
    assert_eq!(stack.stack_status().unwrap().as_str(), "UPDATE_COMPLETE");
    // The instance is updated in place, not replaced.
    assert_eq!(output(stack, "InstanceRef"), id);
    assert_eq!(
        instance_profile_arn(&ec2, &id).await.as_deref(),
        Some("arn:aws:iam::123456789012:instance-profile/admin-profile")
    );
}

#[tokio::test]
async fn cfn_rejects_an_instance_profile_that_is_not_an_instance_profile() {
    // `{"Fn::GetAtt": ["Role", "Arn"]}` resolves to a role ARN, a common
    // template mistake. Storing it made the stack un-updatable later, because
    // the value could not be re-submitted through the validating handler.
    let server = TestServer::start().await;
    let cfn = server.cloudformation_client().await;

    cfn.create_stack()
        .stack_name("bad-profile-stack")
        .template_body(PROFILE_TEMPLATE.replace("PROFILE", "arn:aws:iam::123456789012:role/web"))
        .capabilities(Capability::CapabilityIam)
        .send()
        .await
        .expect("create_stack");

    let described = cfn
        .describe_stacks()
        .stack_name("bad-profile-stack")
        .send()
        .await
        .expect("describe_stacks");
    let stack = described.stacks().first().expect("stack present");
    assert_eq!(stack.stack_status().unwrap().as_str(), "CREATE_FAILED");
}
