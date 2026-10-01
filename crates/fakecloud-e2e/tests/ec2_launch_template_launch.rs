//! Launching from a launch template (#2635): RunInstances resolves the
//! referenced version (`$Default`, `$Latest`, a number; by name or id) and
//! merges its data under the request's own parameters, Auto Scaling launches
//! through the same path for launch templates, mixed-instances policies and
//! launch configurations, and CloudFormation `AWS::EC2::Instance` /
//! `AWS::AutoScaling::AutoScalingGroup` do too. All of it is control plane
//! (instance / volume / tag records), so no container runtime is needed.

mod helpers;

use aws_sdk_ec2::types::{
    BlockDeviceMapping as Ec2Bdm, EbsBlockDevice, Filter, InstanceType,
    LaunchTemplateBlockDeviceMappingRequest, LaunchTemplateEbsBlockDeviceRequest,
    LaunchTemplateIamInstanceProfileSpecificationRequest, LaunchTemplateSpecification,
    LaunchTemplateTagSpecificationRequest, RequestLaunchTemplateData, ResourceType, Tag,
    TagSpecification,
};
use helpers::TestServer;

/// Create the `web` template: version 1 (t3.small, encrypted 30 GiB root,
/// instance + volume tags, IAM instance profile) and version 2 (another AMI
/// and type). The default stays version 1.
async fn create_web_template(ec2: &aws_sdk_ec2::Client) -> String {
    let lt = ec2
        .create_launch_template()
        .launch_template_name("web")
        .launch_template_data(
            RequestLaunchTemplateData::builder()
                .image_id("ami-0a1b2c3d4e5f60001")
                .instance_type(InstanceType::T3Small)
                .key_name("web-key")
                .iam_instance_profile(
                    LaunchTemplateIamInstanceProfileSpecificationRequest::builder()
                        .name("web-profile")
                        .build(),
                )
                .block_device_mappings(
                    LaunchTemplateBlockDeviceMappingRequest::builder()
                        .device_name("/dev/xvda")
                        .ebs(
                            LaunchTemplateEbsBlockDeviceRequest::builder()
                                .volume_size(30)
                                .volume_type(aws_sdk_ec2::types::VolumeType::Gp3)
                                .encrypted(true)
                                .build(),
                        )
                        .build(),
                )
                .tag_specifications(
                    LaunchTemplateTagSpecificationRequest::builder()
                        .resource_type(ResourceType::Instance)
                        .tags(Tag::builder().key("env").value("template").build())
                        .tags(Tag::builder().key("team").value("web").build())
                        .build(),
                )
                .tag_specifications(
                    LaunchTemplateTagSpecificationRequest::builder()
                        .resource_type(ResourceType::Volume)
                        .tags(Tag::builder().key("backup").value("daily").build())
                        .build(),
                )
                .build(),
        )
        .send()
        .await
        .unwrap();
    let id = lt
        .launch_template()
        .unwrap()
        .launch_template_id()
        .unwrap()
        .to_string();
    ec2.create_launch_template_version()
        .launch_template_id(&id)
        .launch_template_data(
            RequestLaunchTemplateData::builder()
                .image_id("ami-0a1b2c3d4e5f60002")
                .instance_type(InstanceType::M5Large)
                .build(),
        )
        .send()
        .await
        .unwrap();
    id
}

async fn describe(ec2: &aws_sdk_ec2::Client, id: &str) -> aws_sdk_ec2::types::Instance {
    ec2.describe_instances()
        .instance_ids(id)
        .send()
        .await
        .unwrap()
        .reservations()[0]
        .instances()[0]
        .clone()
}

fn tag(i: &aws_sdk_ec2::types::Instance, key: &str) -> Option<String> {
    i.tags()
        .iter()
        .find(|t| t.key() == Some(key))
        .and_then(|t| t.value().map(String::from))
}

async fn volumes_of(
    ec2: &aws_sdk_ec2::Client,
    instance_id: &str,
) -> Vec<aws_sdk_ec2::types::Volume> {
    ec2.describe_volumes()
        .filters(
            Filter::builder()
                .name("attachment.instance-id")
                .values(instance_id)
                .build(),
        )
        .send()
        .await
        .unwrap()
        .volumes()
        .to_vec()
}

#[tokio::test]
async fn run_instances_launches_from_launch_template_versions() {
    let s = TestServer::start().await;
    let ec2 = s.ec2_client().await;
    let lt_id = create_web_template(&ec2).await;

    // By name, no version: $Default (version 1), with an InstanceType and a
    // tag override from the request.
    let out = ec2
        .run_instances()
        .min_count(1)
        .max_count(1)
        .launch_template(
            LaunchTemplateSpecification::builder()
                .launch_template_name("web")
                .build(),
        )
        .instance_type(InstanceType::C5Large)
        .tag_specifications(
            TagSpecification::builder()
                .resource_type(ResourceType::Instance)
                .tags(Tag::builder().key("env").value("prod").build())
                .build(),
        )
        .send()
        .await
        .unwrap();
    let id = out.instances()[0].instance_id().unwrap().to_string();
    let i = describe(&ec2, &id).await;
    assert_eq!(i.image_id(), Some("ami-0a1b2c3d4e5f60001"));
    assert_eq!(
        i.instance_type(),
        Some(&InstanceType::C5Large),
        "request wins"
    );
    assert_eq!(i.key_name(), Some("web-key"));
    assert_eq!(tag(&i, "env").as_deref(), Some("prod"), "request tag wins");
    assert_eq!(tag(&i, "team").as_deref(), Some("web"), "template tag kept");
    assert_eq!(
        tag(&i, "aws:ec2launchtemplate:id").as_deref(),
        Some(lt_id.as_str())
    );
    assert_eq!(
        tag(&i, "aws:ec2launchtemplate:version").as_deref(),
        Some("1")
    );
    assert!(i
        .iam_instance_profile()
        .and_then(|p| p.arn())
        .is_some_and(|a| a.ends_with(":instance-profile/web-profile")));
    // The template's encrypted block-device mapping created a real volume
    // with the region's AWS-managed EBS key and the template's volume tags.
    let vols = volumes_of(&ec2, &id).await;
    assert_eq!(vols.len(), 1);
    let v = &vols[0];
    assert_eq!(v.size(), Some(30));
    assert_eq!(v.encrypted(), Some(true));
    assert!(
        v.kms_key_id()
            .is_some_and(|k| k.starts_with("arn:aws:kms:us-east-1:")),
        "aws/ebs key: {:?}",
        v.kms_key_id()
    );
    assert!(v
        .tags()
        .iter()
        .any(|t| t.key() == Some("backup") && t.value() == Some("daily")));

    // $Latest by id, and explicit version numbers.
    for (version, image, itype, n) in [
        (
            "$Latest",
            "ami-0a1b2c3d4e5f60002",
            InstanceType::M5Large,
            "2",
        ),
        ("1", "ami-0a1b2c3d4e5f60001", InstanceType::T3Small, "1"),
        ("2", "ami-0a1b2c3d4e5f60002", InstanceType::M5Large, "2"),
    ] {
        let out = ec2
            .run_instances()
            .min_count(1)
            .max_count(1)
            .launch_template(
                LaunchTemplateSpecification::builder()
                    .launch_template_id(&lt_id)
                    .version(version)
                    .build(),
            )
            .send()
            .await
            .unwrap();
        let i = describe(&ec2, out.instances()[0].instance_id().unwrap()).await;
        assert_eq!(i.image_id(), Some(image), "{version}");
        assert_eq!(i.instance_type(), Some(&itype), "{version}");
        assert_eq!(tag(&i, "aws:ec2launchtemplate:version").as_deref(), Some(n));
    }

    // A request block-device mapping replaces the template's.
    let out = ec2
        .run_instances()
        .min_count(1)
        .max_count(1)
        .launch_template(
            LaunchTemplateSpecification::builder()
                .launch_template_name("web")
                .build(),
        )
        .block_device_mappings(
            Ec2Bdm::builder()
                .device_name("/dev/sdf")
                .ebs(EbsBlockDevice::builder().volume_size(50).build())
                .build(),
        )
        .send()
        .await
        .unwrap();
    let vols = volumes_of(&ec2, out.instances()[0].instance_id().unwrap()).await;
    assert_eq!(vols.len(), 1);
    assert_eq!(vols[0].size(), Some(50));
    assert_eq!(
        vols[0].attachments()[0].device(),
        Some("/dev/sdf"),
        "the request's mapping replaced the template's /dev/xvda"
    );
    assert_eq!(vols[0].encrypted(), Some(false));

    // GetLaunchTemplateData round-trips through the SDK, including the
    // primary interface's security groups (`groupSet` of `groupId`s).
    let data = ec2
        .get_launch_template_data()
        .instance_id(&id)
        .send()
        .await
        .unwrap();
    let data = data.launch_template_data().unwrap();
    assert_eq!(data.image_id(), Some("ami-0a1b2c3d4e5f60001"));
    let groups = data.network_interfaces()[0].groups();
    assert_eq!(
        groups,
        i.security_groups()
            .iter()
            .filter_map(|g| g.group_id().map(String::from))
            .collect::<Vec<_>>()
            .as_slice()
    );
    assert!(!groups.is_empty());
}

/// The error code RunInstances answers a launch-template launch with.
async fn launch_error(
    ec2: &aws_sdk_ec2::Client,
    spec: LaunchTemplateSpecification,
) -> Option<String> {
    let err = ec2
        .run_instances()
        .min_count(1)
        .max_count(1)
        .launch_template(spec)
        .send()
        .await
        .expect_err("launch must fail");
    aws_sdk_ec2::error::ProvideErrorMetadata::code(&err).map(String::from)
}

#[tokio::test]
async fn run_instances_rejects_missing_template_or_version() {
    let s = TestServer::start().await;
    let ec2 = s.ec2_client().await;
    create_web_template(&ec2).await;
    let by_name = |name: &str| {
        LaunchTemplateSpecification::builder()
            .launch_template_name(name)
            .build()
    };
    assert_eq!(
        launch_error(&ec2, by_name("missing")).await.as_deref(),
        Some("InvalidLaunchTemplateName.NotFoundException")
    );
    assert_eq!(
        launch_error(
            &ec2,
            LaunchTemplateSpecification::builder()
                .launch_template_id("lt-0123456789abcdef0")
                .build()
        )
        .await
        .as_deref(),
        Some("InvalidLaunchTemplateId.NotFound")
    );
    assert_eq!(
        launch_error(
            &ec2,
            LaunchTemplateSpecification::builder()
                .launch_template_name("web")
                .version("7")
                .build()
        )
        .await
        .as_deref(),
        Some("InvalidLaunchTemplateId.VersionNotFound")
    );
}

/// The instance ids of an Auto Scaling group.
async fn group_instances(asg: &aws_sdk_autoscaling::Client, name: &str) -> Vec<String> {
    asg.describe_auto_scaling_groups()
        .auto_scaling_group_names(name)
        .send()
        .await
        .unwrap()
        .auto_scaling_groups()[0]
        .instances()
        .iter()
        .filter_map(|i| i.instance_id().map(String::from))
        .collect()
}

#[tokio::test]
async fn auto_scaling_launches_through_launch_templates_and_configurations() {
    let s = TestServer::start().await;
    let ec2 = s.ec2_client().await;
    let asg = aws_sdk_autoscaling::Client::new(&s.aws_config().await);
    create_web_template(&ec2).await;

    // A launch-template group: the template's volume, profile and tags, plus
    // the group's propagate-at-launch tag winning over the template's.
    asg.create_auto_scaling_group()
        .auto_scaling_group_name("lt-group")
        .launch_template(
            aws_sdk_autoscaling::types::LaunchTemplateSpecification::builder()
                .launch_template_name("web")
                .version("$Default")
                .build(),
        )
        .min_size(2)
        .max_size(2)
        .availability_zones("us-east-1a")
        .tags(
            aws_sdk_autoscaling::types::Tag::builder()
                .key("team")
                .value("asg")
                .propagate_at_launch(true)
                .build(),
        )
        .send()
        .await
        .unwrap();
    let ids = group_instances(&asg, "lt-group").await;
    assert_eq!(ids.len(), 2);
    for id in &ids {
        let i = describe(&ec2, id).await;
        assert_eq!(i.instance_type(), Some(&InstanceType::T3Small));
        assert_eq!(tag(&i, "team").as_deref(), Some("asg"));
        assert_eq!(
            tag(&i, "aws:autoscaling:groupName").as_deref(),
            Some("lt-group")
        );
        assert!(i.iam_instance_profile().is_some());
        let vols = volumes_of(&ec2, id).await;
        assert_eq!(vols.len(), 1, "template volume on ASG instance");
        assert_eq!(vols[0].encrypted(), Some(true));
    }

    // A mixed-instances group launches its first override's type.
    asg.create_auto_scaling_group()
        .auto_scaling_group_name("mixed-group")
        .mixed_instances_policy(
            aws_sdk_autoscaling::types::MixedInstancesPolicy::builder()
                .launch_template(
                    aws_sdk_autoscaling::types::LaunchTemplate::builder()
                        .launch_template_specification(
                            aws_sdk_autoscaling::types::LaunchTemplateSpecification::builder()
                                .launch_template_name("web")
                                .version("$Latest")
                                .build(),
                        )
                        .overrides(
                            aws_sdk_autoscaling::types::LaunchTemplateOverrides::builder()
                                .instance_type("c5.xlarge")
                                .build(),
                        )
                        .overrides(
                            aws_sdk_autoscaling::types::LaunchTemplateOverrides::builder()
                                .instance_type("m5.xlarge")
                                .build(),
                        )
                        .build(),
                )
                .build(),
        )
        .min_size(1)
        .max_size(1)
        .availability_zones("us-east-1a")
        .send()
        .await
        .unwrap();
    let ids = group_instances(&asg, "mixed-group").await;
    assert_eq!(ids.len(), 1);
    let i = describe(&ec2, &ids[0]).await;
    assert_eq!(i.instance_type(), Some(&InstanceType::C5Xlarge));
    assert_eq!(i.image_id(), Some("ami-0a1b2c3d4e5f60002"), "$Latest data");

    // A launch-configuration group: the configuration's block devices,
    // instance profile and type.
    asg.create_launch_configuration()
        .launch_configuration_name("lc")
        .image_id("ami-0a1b2c3d4e5f60001")
        .instance_type("m5.large")
        .iam_instance_profile("lc-profile")
        .block_device_mappings(
            aws_sdk_autoscaling::types::BlockDeviceMapping::builder()
                .device_name("/dev/xvda")
                .ebs(
                    aws_sdk_autoscaling::types::Ebs::builder()
                        .volume_size(20)
                        .encrypted(true)
                        .build(),
                )
                .build(),
        )
        .send()
        .await
        .unwrap();
    asg.create_auto_scaling_group()
        .auto_scaling_group_name("lc-group")
        .launch_configuration_name("lc")
        .min_size(1)
        .max_size(1)
        .availability_zones("us-east-1a")
        .send()
        .await
        .unwrap();
    let ids = group_instances(&asg, "lc-group").await;
    assert_eq!(ids.len(), 1);
    let i = describe(&ec2, &ids[0]).await;
    assert_eq!(i.instance_type(), Some(&InstanceType::M5Large));
    assert!(i
        .iam_instance_profile()
        .and_then(|p| p.arn())
        .is_some_and(|a| a.ends_with("/lc-profile")));
    let vols = volumes_of(&ec2, &ids[0]).await;
    assert_eq!(vols.len(), 1);
    assert_eq!(vols[0].size(), Some(20));
    assert_eq!(vols[0].encrypted(), Some(true));
    assert!(vols[0].kms_key_id().is_some(), "aws/ebs key");

    // DescribeLaunchConfigurations reports the mappings.
    let lcs = asg
        .describe_launch_configurations()
        .launch_configuration_names("lc")
        .send()
        .await
        .unwrap();
    let bdm = &lcs.launch_configurations()[0].block_device_mappings()[0];
    assert_eq!(bdm.device_name(), Some("/dev/xvda"));
    assert_eq!(bdm.ebs().and_then(|e| e.volume_size()), Some(20));

    // A group naming a missing launch template is rejected.
    let err = asg
        .create_auto_scaling_group()
        .auto_scaling_group_name("bad")
        .launch_template(
            aws_sdk_autoscaling::types::LaunchTemplateSpecification::builder()
                .launch_template_name("missing")
                .build(),
        )
        .min_size(1)
        .max_size(1)
        .send()
        .await
        .expect_err("missing template");
    assert_eq!(
        aws_sdk_autoscaling::error::ProvideErrorMetadata::code(&err),
        Some("ValidationError")
    );
}

const CFN_TEMPLATE: &str = r#"{
  "Resources": {
    "Tmpl": {
      "Type": "AWS::EC2::LaunchTemplate",
      "Properties": {
        "LaunchTemplateName": "cfn-web",
        "LaunchTemplateData": {
          "ImageId": "ami-0a1b2c3d4e5f60001",
          "InstanceType": "t3.small",
          "BlockDeviceMappings": [
            { "DeviceName": "/dev/xvda", "Ebs": { "VolumeSize": 12, "Encrypted": true } }
          ],
          "TagSpecifications": [
            { "ResourceType": "instance", "Tags": [ { "Key": "tier", "Value": "web" } ] }
          ]
        }
      }
    },
    "Box": {
      "Type": "AWS::EC2::Instance",
      "Properties": {
        "InstanceType": "m5.large",
        "LaunchTemplate": {
          "LaunchTemplateId": { "Ref": "Tmpl" },
          "Version": { "Fn::GetAtt": ["Tmpl", "LatestVersionNumber"] }
        }
      }
    },
    "Group": {
      "Type": "AWS::AutoScaling::AutoScalingGroup",
      "Properties": {
        "AutoScalingGroupName": "cfn-lt-group",
        "MinSize": "1", "MaxSize": "1",
        "AvailabilityZones": ["us-east-1a"],
        "LaunchTemplate": {
          "LaunchTemplateId": { "Ref": "Tmpl" },
          "Version": { "Fn::GetAtt": ["Tmpl", "LatestVersionNumber"] }
        }
      }
    }
  },
  "Outputs": {
    "InstanceId": { "Value": { "Ref": "Box" } }
  }
}"#;

#[tokio::test]
async fn cloudformation_instance_and_group_launch_from_stack_template() {
    let s = TestServer::start().await;
    let cfn = s.cloudformation_client().await;
    let ec2 = s.ec2_client().await;
    let asg = aws_sdk_autoscaling::Client::new(&s.aws_config().await);
    cfn.create_stack()
        .stack_name("lt-stack")
        .template_body(CFN_TEMPLATE)
        .send()
        .await
        .expect("create_stack");
    let stack = cfn
        .describe_stacks()
        .stack_name("lt-stack")
        .send()
        .await
        .unwrap();
    let instance_id = stack.stacks()[0]
        .outputs()
        .iter()
        .find(|o| o.output_key() == Some("InstanceId"))
        .and_then(|o| o.output_value())
        .expect("InstanceId output")
        .to_string();
    let i = describe(&ec2, &instance_id).await;
    assert_eq!(i.image_id(), Some("ami-0a1b2c3d4e5f60001"));
    assert_eq!(
        i.instance_type(),
        Some(&InstanceType::M5Large),
        "resource wins"
    );
    assert_eq!(tag(&i, "tier").as_deref(), Some("web"));
    let vols = volumes_of(&ec2, &instance_id).await;
    assert_eq!(vols.len(), 1);
    assert_eq!(vols[0].size(), Some(12));
    assert_eq!(vols[0].encrypted(), Some(true));

    // The stack's group reconciles in the background; its instance gets the
    // template's volume too.
    let ids = helpers::wait_until(std::time::Duration::from_secs(10), || {
        let asg = asg.clone();
        async move {
            let ids = group_instances(&asg, "cfn-lt-group").await;
            (!ids.is_empty()).then_some(ids)
        }
    })
    .await
    .expect("CFN group launched its instance");
    let vols = volumes_of(&ec2, &ids[0]).await;
    assert_eq!(vols.len(), 1);
    assert_eq!(vols[0].size(), Some(12));
}
