//! Application Auto Scaling ECS desired-count scaling E2E.
//!
//! Verifies the scaling watcher:
//!   - TargetTracking scales ECS service desiredCount based on published
//!     CloudWatch ECSServiceAverageCPUUtilization metric.
//!   - StepScaling scales ECS service desiredCount when a CloudWatch alarm
//!     wired to the policy's ARN transitions to ALARM.
//!   - ScalingActivity rows are emitted for both paths.

mod helpers;

use aws_sdk_applicationautoscaling::types::{ScalableDimension, ServiceNamespace};
use aws_sdk_cloudwatch::types::{Dimension, MetricDatum};
use aws_sdk_ecs::types::ContainerDefinition;
use helpers::TestServer;

async fn bootstrap_ecs_service(
    ecs: &aws_sdk_ecs::Client,
    cluster: &str,
    family: &str,
    service: &str,
    desired_count: i32,
) {
    ecs.create_cluster()
        .cluster_name(cluster)
        .send()
        .await
        .expect("create cluster");
    ecs.register_task_definition()
        .family(family)
        .container_definitions(
            ContainerDefinition::builder()
                .name("app")
                .image("public.ecr.aws/docker/library/alpine:latest")
                .essential(true)
                .build(),
        )
        .send()
        .await
        .expect("register td");
    ecs.create_service()
        .cluster(cluster)
        .service_name(service)
        .task_definition(family)
        .desired_count(desired_count)
        .send()
        .await
        .expect("create service");
}

async fn force_watcher_tick(server: &TestServer) -> usize {
    let url = format!(
        "{}/_fakecloud/application-autoscaling/tick",
        server.endpoint()
    );
    let resp = reqwest::Client::new()
        .post(&url)
        .send()
        .await
        .expect("admin tick");
    let body: serde_json::Value = resp.json().await.expect("json");
    body["applied"].as_u64().unwrap_or(0) as usize
}

#[tokio::test]
async fn target_tracking_scales_ecs_service_desired_count() {
    let server = TestServer::start().await;
    let ecs = server.ecs_client().await;
    let aas = server.application_autoscaling_client().await;
    let cw = server.cloudwatch_client().await;

    bootstrap_ecs_service(&ecs, "appas-ecs-cluster", "appas-ecs-td", "web", 1).await;

    aas.register_scalable_target()
        .service_namespace(ServiceNamespace::Ecs)
        .resource_id("service/appas-ecs-cluster/web")
        .scalable_dimension(ScalableDimension::EcsServiceDesiredCount)
        .min_capacity(1)
        .max_capacity(5)
        .send()
        .await
        .expect("register scalable target");

    let policy_resp = aas
        .put_scaling_policy()
        .service_namespace(ServiceNamespace::Ecs)
        .resource_id("service/appas-ecs-cluster/web")
        .scalable_dimension(ScalableDimension::EcsServiceDesiredCount)
        .policy_name("cpu-tracking")
        .policy_type(aws_sdk_applicationautoscaling::types::PolicyType::TargetTrackingScaling)
        .target_tracking_scaling_policy_configuration(
            aws_sdk_applicationautoscaling::types::TargetTrackingScalingPolicyConfiguration::builder()
                .target_value(70.0)
                .predefined_metric_specification(
                    aws_sdk_applicationautoscaling::types::PredefinedMetricSpecification::builder()
                        .predefined_metric_type(
                            aws_sdk_applicationautoscaling::types::MetricType::EcsServiceAverageCpuUtilization,
                        )
                        .build()
                        .expect("build predefined metric spec"),
                )
                .scale_out_cooldown(0)
                .scale_in_cooldown(0)
                .build()
                .expect("build target tracking config"),
        )
        .send()
        .await
        .expect("put scaling policy");
    assert!(!policy_resp.policy_arn().is_empty(), "expected policy ARN");

    // Publish high CPU metric to trigger scale-out.
    cw.put_metric_data()
        .namespace("AWS/ECS")
        .metric_data(
            MetricDatum::builder()
                .metric_name("ECSServiceAverageCPUUtilization")
                .dimensions(
                    Dimension::builder()
                        .name("ServiceName")
                        .value("web")
                        .build(),
                )
                .dimensions(
                    Dimension::builder()
                        .name("ClusterName")
                        .value("appas-ecs-cluster")
                        .build(),
                )
                .value(90.0)
                .build(),
        )
        .send()
        .await
        .expect("put metric data");

    let applied = force_watcher_tick(&server).await;
    assert_eq!(applied, 1, "expected scaling policy to apply");

    // Verify ECS service desiredCount scaled up.
    // utilisation 90, target 70 => factor 1.28, current 1 => ceil = 2.
    let described = ecs
        .describe_services()
        .cluster("appas-ecs-cluster")
        .services("web")
        .send()
        .await
        .expect("describe services");
    let svc = described.services().first().expect("service");
    assert_eq!(
        svc.desired_count(),
        2,
        "expected desired_count=2 after scale-out"
    );

    // Verify scaling activity.
    let activities = aas
        .describe_scaling_activities()
        .service_namespace(ServiceNamespace::Ecs)
        .resource_id("service/appas-ecs-cluster/web")
        .scalable_dimension(ScalableDimension::EcsServiceDesiredCount)
        .send()
        .await
        .expect("activities")
        .scaling_activities()
        .to_vec();
    assert!(
        activities.iter().any(|a| {
            a.description().contains("desired count") && a.status_code().as_str() == "Successful"
        }),
        "expected Successful scaling activity, got {activities:?}"
    );
}

#[tokio::test]
async fn step_scaling_scales_ecs_service_desired_count() {
    let server = TestServer::start().await;
    let ecs = server.ecs_client().await;
    let aas = server.application_autoscaling_client().await;
    let cw = server.cloudwatch_client().await;

    bootstrap_ecs_service(
        &ecs,
        "appas-ecs-step-cluster",
        "appas-ecs-step-td",
        "web",
        1,
    )
    .await;

    aas.register_scalable_target()
        .service_namespace(ServiceNamespace::Ecs)
        .resource_id("service/appas-ecs-step-cluster/web")
        .scalable_dimension(ScalableDimension::EcsServiceDesiredCount)
        .min_capacity(1)
        .max_capacity(5)
        .send()
        .await
        .expect("register scalable target");

    let policy_resp = aas
        .put_scaling_policy()
        .service_namespace(ServiceNamespace::Ecs)
        .resource_id("service/appas-ecs-step-cluster/web")
        .scalable_dimension(ScalableDimension::EcsServiceDesiredCount)
        .policy_name("step-up")
        .policy_type(aws_sdk_applicationautoscaling::types::PolicyType::StepScaling)
        .step_scaling_policy_configuration(
            aws_sdk_applicationautoscaling::types::StepScalingPolicyConfiguration::builder()
                .adjustment_type(
                    aws_sdk_applicationautoscaling::types::AdjustmentType::ChangeInCapacity,
                )
                .cooldown(0)
                .step_adjustments(
                    aws_sdk_applicationautoscaling::types::StepAdjustment::builder()
                        .metric_interval_lower_bound(0.0)
                        .scaling_adjustment(2)
                        .build()
                        .expect("build step adjustment"),
                )
                .build(),
        )
        .send()
        .await
        .expect("put scaling policy");
    let policy_arn = policy_resp.policy_arn();

    // Create alarm with the policy ARN as an action.
    cw.put_metric_alarm()
        .alarm_name("ecs-high-cpu")
        .metric_name("ECSServiceAverageCPUUtilization")
        .namespace("AWS/ECS")
        .statistic(aws_sdk_cloudwatch::types::Statistic::Average)
        .period(60)
        .evaluation_periods(1)
        .threshold(80.0)
        .comparison_operator(aws_sdk_cloudwatch::types::ComparisonOperator::GreaterThanThreshold)
        .alarm_actions(policy_arn)
        .send()
        .await
        .expect("put metric alarm");

    // Force alarm to ALARM.
    cw.set_alarm_state()
        .alarm_name("ecs-high-cpu")
        .state_value(aws_sdk_cloudwatch::types::StateValue::Alarm)
        .state_reason("test")
        .send()
        .await
        .expect("set alarm state");

    let applied = force_watcher_tick(&server).await;
    assert_eq!(applied, 1, "expected step scaling to apply");

    // ChangeInCapacity +2 from current 1 => desired 3.
    let described = ecs
        .describe_services()
        .cluster("appas-ecs-step-cluster")
        .services("web")
        .send()
        .await
        .expect("describe services");
    let svc = described.services().first().expect("service");
    assert_eq!(
        svc.desired_count(),
        3,
        "expected desired_count=3 after step scaling"
    );

    // Verify scaling activity.
    let activities = aas
        .describe_scaling_activities()
        .service_namespace(ServiceNamespace::Ecs)
        .resource_id("service/appas-ecs-step-cluster/web")
        .scalable_dimension(ScalableDimension::EcsServiceDesiredCount)
        .send()
        .await
        .expect("activities")
        .scaling_activities()
        .to_vec();
    assert!(
        activities.iter().any(|a| {
            a.description().contains("desired count") && a.status_code().as_str() == "Successful"
        }),
        "expected Successful scaling activity, got {activities:?}"
    );
}
