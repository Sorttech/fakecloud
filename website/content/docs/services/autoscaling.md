+++
title = "EC2 Auto Scaling"
description = "Amazon EC2 Auto Scaling — Auto Scaling Groups, Launch Configurations, desired-capacity reconciliation, and scaling activities. Query protocol."
weight = 28
+++

EC2 Auto Scaling (the `autoscaling` service) manages EC2 fleets — distinct from
[Application Auto Scaling](/docs/services/application-autoscaling/) (the
`application-autoscaling` service), which scales DynamoDB / ECS / etc. targets.

The wedge: against every other free local emulator an Auto Scaling Group scales
to a *mock* instance (LocalStack #8367 "ASG launches a mock EC2 instead of a
Docker instance"; MiniStack's ASG is a Terraform-timeout stub). fakecloud runs
EC2 instances as real containers, so an ASG reconciled to its desired capacity
launches *real* instances.

## Supported today

- **Launch Configurations** — `CreateLaunchConfiguration`, `DescribeLaunchConfigurations`, `DeleteLaunchConfiguration`, including `BlockDeviceMappings` and `MetadataOptions`.
- **Auto Scaling Groups** — `CreateAutoScalingGroup`, `DescribeAutoScalingGroups`, `UpdateAutoScalingGroup`, `DeleteAutoScalingGroup` (rejects delete with instances unless `ForceDelete`). The launch source is exactly one of a Launch Configuration, an EC2 Launch Template, an `InstanceId` (a launch configuration named after the group is derived from the instance: AMI, type, key pair, security groups, user data, IAM instance profile, volumes, and its subnet or AZ when the group names none), or a `MixedInstancesPolicy` (launch template + `Overrides` with `InstanceType`, `WeightedCapacity` and per-override `LaunchTemplateSpecification`, plus its `InstancesDistribution`). A launch configuration that does not exist, or a launch template / version that does not resolve, is rejected with `ValidationError`; a resolved template is reported with both its id and name, and `$Default` when no version was given.
- **Capacity** — `SetDesiredCapacity` and the `DesiredCapacity` on create/update reconcile the group's instance set, launching **real container-backed EC2 instances** or terminating them on scale-in, and recording a `Successful` scaling activity for each. Every instance launches through EC2 `RunInstances`, so it gets exactly what a direct launch would: a launch template's resolved version (image, type, key pair, security groups, network interfaces, user data, IAM instance profile, block-device mappings as real EBS volumes, tags, metadata options, ...), or a launch configuration's equivalent (`BlockDeviceMappings`, `IamInstanceProfile`, `UserData`, `SecurityGroups`, `KeyName`, monitoring, tenancy, metadata options). Encrypted volumes get the region's `aws/ebs` key. A mixed-instances group launches its highest-priority override (the `prioritized` strategy) and counts `WeightedCapacity` toward the desired capacity. Instances are spread across the `VPCZoneIdentifier` subnets (or the group's availability zones), carry the `aws:autoscaling:groupName` tag and the group's `PropagateAtLaunch` tags (which win over same-key launch-template tags), and report their `InstanceType`, `LaunchTemplate` (with the concrete version) and `WeightedCapacity`. A launch that cannot resolve its template records a `Failed` scaling activity instead of a phantom instance. The launched instances show up in EC2 `DescribeInstances` and `DescribeVolumes`, unlike every free rival, whose ASG scales to a mock instance (LocalStack #8367).
- **Activities** — `DescribeScalingActivities` returns the launch/terminate activities (this is the op Terraform's `aws_autoscaling_group` create blocks on, and the one MiniStack lacks — #331).
- **Instances** — `DescribeAutoScalingInstances` reports each group's instances with `LifecycleState` / `HealthStatus`.
- **Tags** — `CreateOrUpdateTags`, `DeleteTags`, `DescribeTags` (with `PropagateAtLaunch`).

## Protocol

AWS Query protocol (form-encoded request, `<ActionResponse>...<ResponseMetadata>`
XML response), endpoint `autoscaling.<region>.amazonaws.com`.

## Roadmap

- Scaling policies (target-tracking / step), lifecycle hooks, instance refresh, and conformance-harness coverage.
