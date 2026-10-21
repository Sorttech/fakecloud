//! CloudFormation provisioner for AWS::ECS::Cluster + TaskDefinition + Service + CapacityProvider.

mod helpers;

use aws_sdk_cloudformation::types::{Capability, OnFailure, Parameter};
use helpers::TestServer;

const TEMPLATE: &str = r#"{
  "AWSTemplateFormatVersion": "2010-09-09",
  "Resources": {
    "Cluster": {
      "Type": "AWS::ECS::Cluster",
      "Properties": {
        "ClusterName": "cfn-ecs-cluster",
        "ClusterSettings": [{"Name": "containerInsights", "Value": "enhanced"}],
        "ServiceConnectDefaults": {"Namespace": "cfn-ns"},
        "Tags": [{"Key": "env", "Value": "test"}]
      }
    },
    "CP": {
      "Type": "AWS::ECS::CapacityProvider",
      "Properties": {
        "Name": "cfn-cp"
      }
    },
    "TaskDef": {
      "Type": "AWS::ECS::TaskDefinition",
      "Properties": {
        "Family": "cfn-task",
        "NetworkMode": "awsvpc",
        "RequiresCompatibilities": ["FARGATE"],
        "Cpu": "256",
        "Memory": "512",
        "EphemeralStorage": {"SizeInGiB": 30},
        "RuntimePlatform": {"OperatingSystemFamily": "LINUX", "CpuArchitecture": "X86_64"},
        "ContainerDefinitions": [
          {
            "Name": "web",
            "Image": "nginx:alpine",
            "Essential": true,
            "PortMappings": [{"ContainerPort": 80, "Protocol": "tcp"}]
          }
        ]
      }
    },
    "Svc": {
      "Type": "AWS::ECS::Service",
      "Properties": {
        "ServiceName": "cfn-svc",
        "Cluster": {"Ref": "Cluster"},
        "TaskDefinition": {"Ref": "TaskDef"},
        "DesiredCount": 1,
        "LaunchType": "FARGATE",
        "PlatformVersion": "1.4.0",
        "HealthCheckGracePeriodSeconds": 60,
        "EnableECSManagedTags": true,
        "EnableExecuteCommand": true,
        "PropagateTags": "SERVICE",
        "AvailabilityZoneRebalancing": "ENABLED",
        "DeploymentConfiguration": {
          "MinimumHealthyPercent": 50,
          "MaximumPercent": 200
        }
      }
    }
  },
  "Outputs": {
    "ClusterName": {"Value": {"Ref": "Cluster"}},
    "ClusterArn": {"Value": {"Fn::GetAtt": ["Cluster", "Arn"]}},
    "CpName": {"Value": {"Ref": "CP"}},
    "CpArn": {"Value": {"Fn::GetAtt": ["CP", "Arn"]}},
    "TaskDefArn": {"Value": {"Ref": "TaskDef"}},
    "ServiceArn": {"Value": {"Fn::GetAtt": ["Svc", "ServiceArn"]}},
    "ServiceName": {"Value": {"Fn::GetAtt": ["Svc", "Name"]}}
  }
}"#;

const TEMPLATE_UPDATE: &str = r#"{
  "AWSTemplateFormatVersion": "2010-09-09",
  "Parameters": {
    "Desired": {"Type": "Number", "Default": 3}
  },
  "Resources": {
    "Cluster": {
      "Type": "AWS::ECS::Cluster",
      "Properties": {
        "ClusterName": "cfn-ecs-cluster",
        "ClusterSettings": [{"Name": "containerInsights", "Value": "disabled"}],
        "ServiceConnectDefaults": {"Namespace": "cfn-ns-2"},
        "Tags": [{"Key": "env", "Value": "prod"}]
      }
    },
    "CP": {
      "Type": "AWS::ECS::CapacityProvider",
      "Properties": {
        "Name": "cfn-cp",
        "Tags": [{"Key": "tier", "Value": "compute"}]
      }
    },
    "TaskDef": {
      "Type": "AWS::ECS::TaskDefinition",
      "Properties": {
        "Family": "cfn-task",
        "NetworkMode": "awsvpc",
        "RequiresCompatibilities": ["FARGATE"],
        "Cpu": "256",
        "Memory": "512",
        "ContainerDefinitions": [
          {
            "Name": "web",
            "Image": "nginx:alpine",
            "Essential": true,
            "PortMappings": [{"ContainerPort": 80, "Protocol": "tcp"}]
          }
        ]
      }
    },
    "Svc": {
      "Type": "AWS::ECS::Service",
      "Properties": {
        "ServiceName": "cfn-svc",
        "Cluster": {"Ref": "Cluster"},
        "TaskDefinition": {"Ref": "TaskDef"},
        "DesiredCount": {"Ref": "Desired"},
        "LaunchType": "FARGATE",
        "PlatformVersion": "LATEST",
        "HealthCheckGracePeriodSeconds": 120,
        "EnableExecuteCommand": false,
        "DeploymentConfiguration": {
          "MinimumHealthyPercent": 100,
          "MaximumPercent": 200
        }
      }
    }
  },
  "Outputs": {
    "ServiceArn": {"Value": {"Fn::GetAtt": ["Svc", "ServiceArn"]}}
  }
}"#;

#[tokio::test]
async fn cfn_provisions_ecs_cluster_taskdef_service_capacity_provider() {
    let server = TestServer::start().await;
    let cfn = server.cloudformation_client().await;
    let ecs = aws_sdk_ecs::Client::new(&server.aws_config().await);

    cfn.create_stack()
        .stack_name("ecs-stack")
        .template_body(TEMPLATE)
        .capabilities(Capability::CapabilityIam)
        .on_failure(OnFailure::Rollback)
        .send()
        .await
        .expect("create_stack");

    let described = cfn
        .describe_stacks()
        .stack_name("ecs-stack")
        .send()
        .await
        .expect("describe_stacks");
    let stack = described.stacks().first().expect("stack present");
    assert_eq!(stack.stack_status().unwrap().as_str(), "CREATE_COMPLETE");

    let mut outs = std::collections::HashMap::new();
    for o in stack.outputs() {
        if let (Some(k), Some(v)) = (o.output_key(), o.output_value()) {
            outs.insert(k.to_string(), v.to_string());
        }
    }
    assert_eq!(
        outs.get("ClusterName").map(|s| s.as_str()),
        Some("cfn-ecs-cluster")
    );
    assert!(outs
        .get("ClusterArn")
        .unwrap()
        .contains(":cluster/cfn-ecs-cluster"));
    assert_eq!(outs.get("CpName").map(|s| s.as_str()), Some("cfn-cp"));
    assert!(outs
        .get("CpArn")
        .unwrap()
        .contains(":capacity-provider/cfn-cp"));
    assert!(outs
        .get("TaskDefArn")
        .unwrap()
        .contains(":task-definition/cfn-task:1"));
    assert!(outs
        .get("ServiceArn")
        .unwrap()
        .contains(":service/cfn-ecs-cluster/cfn-svc"));

    // Verify cluster + service via SDK.
    let clusters = ecs
        .describe_clusters()
        .clusters("cfn-ecs-cluster")
        .send()
        .await
        .expect("describe_clusters");
    assert_eq!(
        clusters.clusters().first().and_then(|c| c.cluster_name()),
        Some("cfn-ecs-cluster")
    );

    let svcs = ecs
        .describe_services()
        .cluster("cfn-ecs-cluster")
        .services("cfn-svc")
        .send()
        .await
        .expect("describe_services");
    let svc = svcs.services().first().expect("service present");
    assert_eq!(svc.service_name(), Some("cfn-svc"));
    assert_eq!(svc.desired_count(), 1);
    // CFN-only properties round-trip through DescribeServices.
    assert_eq!(svc.platform_version(), Some("1.4.0"));
    assert_eq!(svc.health_check_grace_period_seconds(), Some(60));
    assert!(svc.enable_ecs_managed_tags());
    assert!(svc.enable_execute_command());
    assert_eq!(svc.propagate_tags().map(|p| p.as_str()), Some("SERVICE"));
    assert_eq!(
        svc.availability_zone_rebalancing().map(|a| a.as_str()),
        Some("ENABLED")
    );
    assert_eq!(outs.get("ServiceName").map(|s| s.as_str()), Some("cfn-svc"));

    let td = ecs
        .describe_task_definition()
        .task_definition("cfn-task:1")
        .send()
        .await
        .expect("describe_task_definition");
    let td_inner = td.task_definition().expect("task definition present");
    assert_eq!(td_inner.family(), Some("cfn-task"));
    // EphemeralStorage round-trips.
    assert_eq!(
        td_inner.ephemeral_storage().map(|e| e.size_in_gib()),
        Some(30)
    );

    // Cluster ServiceConnectDefaults round-trips.
    let cluster = clusters.clusters().first().expect("cluster present");
    let scd_namespace = cluster
        .service_connect_defaults()
        .map(|s| s.namespace().unwrap_or(""));
    assert_eq!(scd_namespace, Some("cfn-ns"));

    // --- UpdateStack: bumps desired count, swaps task definition. ---
    cfn.update_stack()
        .stack_name("ecs-stack")
        .template_body(TEMPLATE_UPDATE)
        .capabilities(Capability::CapabilityIam)
        .parameters(
            Parameter::builder()
                .parameter_key("Desired")
                .parameter_value("3")
                .build(),
        )
        .send()
        .await
        .expect("update_stack");

    let described = cfn
        .describe_stacks()
        .stack_name("ecs-stack")
        .send()
        .await
        .expect("describe_stacks after update");
    let stack = described.stacks().first().expect("stack present");
    assert_eq!(stack.stack_status().unwrap().as_str(), "UPDATE_COMPLETE");

    let svcs = ecs
        .describe_services()
        .cluster("cfn-ecs-cluster")
        .services("cfn-svc")
        .send()
        .await
        .expect("describe_services after update");
    let svc = svcs
        .services()
        .first()
        .expect("service present after update");
    assert_eq!(svc.desired_count(), 3);
    assert_eq!(svc.platform_version(), Some("LATEST"));
    assert_eq!(svc.health_check_grace_period_seconds(), Some(120));
    assert!(!svc.enable_execute_command());
    // The task definition's properties changed (EphemeralStorage and
    // RuntimePlatform were removed), so CloudFormation registers a new
    // revision and updates the service — which `Ref`s the task definition —
    // to point at it. With dependency-ordered provisioning the service is
    // resolved after the task definition, so it picks up the new revision
    // (cfn-task:2) rather than the stale cfn-task:1.
    assert!(
        svc.task_definition().unwrap().contains("cfn-task:2"),
        "service should reference the re-registered task definition revision, got {:?}",
        svc.task_definition()
    );

    // Cluster Tag was rewritten by update.
    let after_clusters = ecs
        .describe_clusters()
        .clusters("cfn-ecs-cluster")
        .include(aws_sdk_ecs::types::ClusterField::Tags)
        .send()
        .await
        .expect("describe_clusters after update");
    let after_cluster = after_clusters
        .clusters()
        .first()
        .expect("cluster present after update");
    let env_tag = after_cluster
        .tags()
        .iter()
        .find(|t| t.key() == Some("env"))
        .and_then(|t| t.value());
    assert_eq!(env_tag, Some("prod"));

    cfn.delete_stack()
        .stack_name("ecs-stack")
        .send()
        .await
        .expect("delete_stack");

    // Cluster gone after stack delete.
    let after = ecs
        .describe_clusters()
        .clusters("cfn-ecs-cluster")
        .send()
        .await
        .expect("describe_clusters after delete");
    let still_active = after
        .clusters()
        .iter()
        .any(|c| c.cluster_name() == Some("cfn-ecs-cluster") && c.status() == Some("ACTIVE"));
    assert!(
        !still_active,
        "cluster should be gone or non-active after stack deletion"
    );
}

fn rolling_template(version: &str) -> String {
    format!(
        r#"{{
  "AWSTemplateFormatVersion": "2010-09-09",
  "Resources": {{
    "Cluster": {{ "Type": "AWS::ECS::Cluster", "Properties": {{ "ClusterName": "roll-cluster" }} }},
    "TaskDef": {{
      "Type": "AWS::ECS::TaskDefinition",
      "Properties": {{
        "Family": "roll-task",
        "NetworkMode": "awsvpc",
        "RequiresCompatibilities": ["FARGATE"],
        "Cpu": "256",
        "Memory": "512",
        "ContainerDefinitions": [{{
          "Name": "app",
          "Image": "redis:7-alpine",
          "Essential": true,
          "Environment": [{{"Name": "RELEASE", "Value": "{version}"}}]
        }}]
      }}
    }},
    "Svc": {{
      "Type": "AWS::ECS::Service",
      "Properties": {{
        "ServiceName": "roll-svc",
        "Cluster": {{"Ref": "Cluster"}},
        "TaskDefinition": {{"Ref": "TaskDef"}},
        "DesiredCount": 1,
        "LaunchType": "FARGATE"
      }}
    }}
  }}
}}"#
    )
}

/// A CloudFormation update that changes a service's task definition must replace its tasks: start a
/// replacement beside the old one (`maximumPercent` allows it), stop the old one once the new one
/// serves (`minimumHealthyPercent` allows it), and end with one deployment, `COMPLETED`, whose
/// counts are real. Before this the old deployment was relabelled `ACTIVE` and nothing else
/// happened: the old task ran on for ever, no replacement started, and the deployment counts only
/// ever grew
/// (<https://docs.aws.amazon.com/AmazonECS/latest/developerguide/deployment-type-ecs.md>, fetched
/// 2026-10-21).
#[tokio::test]
async fn a_cloudformation_task_definition_update_rolls_the_service_to_the_new_revision() {
    let docker_ok = std::process::Command::new("docker")
        .arg("info")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !docker_ok {
        assert!(std::env::var("CI").is_err(), "docker is required in CI");
        eprintln!("Skipping: docker not available");
        return;
    }
    let server = TestServer::start().await;
    let cfn = server.cloudformation_client().await;
    let ecs = server.ecs_client().await;

    cfn.create_stack()
        .stack_name("roll-stack")
        .template_body(rolling_template("1"))
        .send()
        .await
        .expect("create_stack");

    let service = || {
        let ecs = ecs.clone();
        async move {
            ecs.describe_services()
                .cluster("roll-cluster")
                .services("roll-svc")
                .send()
                .await
                .expect("describe_services")
                .services()[0]
                .clone()
        }
    };
    let revision = |arn: &str| arn.rsplit(':').next().unwrap_or("").to_string();

    // Wait for the first task to be serving.
    let mut first_revision = String::new();
    for _ in 0..90 {
        let svc = service().await;
        if svc.running_count() == 1 {
            first_revision = revision(svc.task_definition().unwrap());
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    assert!(
        !first_revision.is_empty(),
        "the first task never reached RUNNING"
    );

    cfn.update_stack()
        .stack_name("roll-stack")
        .template_body(rolling_template("2"))
        .send()
        .await
        .expect("update_stack");

    // The service ends on the new revision with exactly one deployment, completed, with real counts.
    let mut settled = None;
    for _ in 0..150 {
        let svc = service().await;
        let current = revision(svc.task_definition().unwrap());
        let deployments = svc.deployments();
        if current != first_revision
            && deployments.len() == 1
            && deployments[0].status() == Some("PRIMARY")
            && deployments[0].rollout_state().map(|s| s.as_str()) == Some("COMPLETED")
            && deployments[0].running_count() == 1
        {
            settled = Some(svc);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    let svc = settled.expect("the rollout never completed on the new task definition");
    assert_eq!(svc.deployments()[0].pending_count(), 0);
    assert_eq!(svc.running_count(), 1);
    assert_eq!(svc.pending_count(), 0);

    // And the old task is really gone: the only RUNNING task is on the new revision.
    let running = ecs
        .list_tasks()
        .cluster("roll-cluster")
        .desired_status(aws_sdk_ecs::types::DesiredStatus::Running)
        .send()
        .await
        .expect("list_tasks");
    assert_eq!(running.task_arns().len(), 1);
    let described = ecs
        .describe_tasks()
        .cluster("roll-cluster")
        .tasks(&running.task_arns()[0])
        .send()
        .await
        .expect("describe_tasks");
    let task_definition = described.tasks()[0].task_definition_arn().unwrap();
    assert_ne!(revision(task_definition), first_revision);
}
