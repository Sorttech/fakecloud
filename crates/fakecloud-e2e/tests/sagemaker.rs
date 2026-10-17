//! End-to-end tests for the Amazon SageMaker control plane, driven through the
//! real `aws-sdk-sagemaker` client against a live fakecloud server. Exercises
//! the model / endpoint-config control plane and tagging end to end: create a
//! model -> describe / list it (round-tripping the execution role ARN and
//! asserting the server-generated `CreationTime` epoch-second timestamp
//! deserialises), create an endpoint config, and tag / list-tags / delete-tags
//! a resource by ARN.

use aws_sdk_sagemaker::types::{ContainerDefinition, ProductionVariant, Tag};
use fakecloud_testkit::TestServer;

async fn sagemaker_client(server: &TestServer) -> aws_sdk_sagemaker::Client {
    let conf = aws_sdk_sagemaker::config::Builder::from(&server.aws_config().await).build();
    aws_sdk_sagemaker::Client::from_conf(conf)
}

#[tokio::test]
async fn sagemaker_control_plane_lifecycle() {
    let server = TestServer::start().await;
    let client = sagemaker_client(&server).await;

    // --- Model (name-addressed) ---
    let role = "arn:aws:iam::000000000000:role/SageMakerRole";
    let created = client
        .create_model()
        .model_name("my-model")
        .execution_role_arn(role)
        .primary_container(
            ContainerDefinition::builder()
                .image("123456789012.dkr.ecr.us-east-1.amazonaws.com/my-image:latest")
                .build(),
        )
        .send()
        .await
        .expect("create_model");
    let model_arn = created.model_arn().expect("model_arn").to_string();
    assert!(
        model_arn.contains(":model/my-model"),
        "unexpected model arn: {model_arn}"
    );

    // Describe round-trips the execution role and carries a numeric CreationTime.
    let described = client
        .describe_model()
        .model_name("my-model")
        .send()
        .await
        .expect("describe_model");
    assert_eq!(described.model_name(), Some("my-model"));
    assert_eq!(described.execution_role_arn(), Some(role));
    assert_eq!(described.model_arn(), Some(model_arn.as_str()));
    // The awsJson1.1 epoch-second timestamp must deserialise into a DateTime.
    let creation = described.creation_time().expect("creation_time present");
    assert!(creation.secs() > 0, "creation time should be a real epoch");

    // List surfaces the model summary with its required Name/Arn/CreationTime.
    let listed = client.list_models().send().await.expect("list_models");
    assert!(listed
        .models()
        .iter()
        .any(|m| m.model_name() == Some("my-model")
            && m.model_arn().unwrap_or_default().contains("my-model")));

    // --- Endpoint config ---
    client
        .create_endpoint_config()
        .endpoint_config_name("my-endpoint-config")
        .production_variants(
            ProductionVariant::builder()
                .variant_name("AllTraffic")
                .model_name("my-model")
                .initial_instance_count(1)
                .instance_type(aws_sdk_sagemaker::types::ProductionVariantInstanceType::MlM5Large)
                .build(),
        )
        .send()
        .await
        .expect("create_endpoint_config");
    let described_cfg = client
        .describe_endpoint_config()
        .endpoint_config_name("my-endpoint-config")
        .send()
        .await
        .expect("describe_endpoint_config");
    assert_eq!(
        described_cfg.endpoint_config_name(),
        Some("my-endpoint-config")
    );
    assert!(described_cfg
        .endpoint_config_arn()
        .unwrap_or_default()
        .contains("my-endpoint-config"));

    // --- Tagging by ARN ---
    client
        .add_tags()
        .resource_arn(&model_arn)
        .tags(Tag::builder().key("env").value("prod").build())
        .send()
        .await
        .expect("add_tags");
    let tags = client
        .list_tags()
        .resource_arn(&model_arn)
        .send()
        .await
        .expect("list_tags");
    assert!(tags
        .tags()
        .iter()
        .any(|t| t.key() == Some("env") && t.value() == Some("prod")));

    client
        .delete_tags()
        .resource_arn(&model_arn)
        .tag_keys("env")
        .send()
        .await
        .expect("delete_tags");
    let tags = client
        .list_tags()
        .resource_arn(&model_arn)
        .send()
        .await
        .expect("list_tags after delete");
    assert!(tags.tags().is_empty(), "tags should be cleared");

    // Delete the model; describing it afterwards is a ResourceNotFound.
    client
        .delete_model()
        .model_name("my-model")
        .send()
        .await
        .expect("delete_model");
    let err = client
        .describe_model()
        .model_name("my-model")
        .send()
        .await
        .expect_err("describe after delete should fail");
    assert!(format!("{err:?}").contains("ResourceNotFound"));
}

/// `StartPipelineExecution` is the only creation path for a pipeline execution;
/// the started execution must then be resolvable by Describe / List.
#[tokio::test]
async fn sagemaker_pipeline_execution_round_trip() {
    let server = TestServer::start().await;
    let client = sagemaker_client(&server).await;

    let started = client
        .start_pipeline_execution()
        .pipeline_name("my-pipeline")
        .pipeline_execution_display_name("run-1")
        .send()
        .await
        .expect("start_pipeline_execution");
    let exec_arn = started
        .pipeline_execution_arn()
        .expect("pipeline_execution_arn")
        .to_string();
    assert!(
        exec_arn.contains(":pipeline/my-pipeline/execution/"),
        "unexpected execution arn: {exec_arn}"
    );

    // Describe resolves the started execution by its minted ARN.
    let described = client
        .describe_pipeline_execution()
        .pipeline_execution_arn(&exec_arn)
        .send()
        .await
        .expect("describe_pipeline_execution");
    assert_eq!(described.pipeline_execution_arn(), Some(exec_arn.as_str()));
    assert!(described
        .pipeline_arn()
        .unwrap_or_default()
        .contains(":pipeline/my-pipeline"));

    // List surfaces the execution summary for the pipeline.
    let listed = client
        .list_pipeline_executions()
        .pipeline_name("my-pipeline")
        .send()
        .await
        .expect("list_pipeline_executions");
    assert!(
        listed
            .pipeline_execution_summaries()
            .iter()
            .any(|s| s.pipeline_execution_arn() == Some(exec_arn.as_str())),
        "started execution should appear in the summaries"
    );
}

/// Stop*/Start* lifecycle ops must advance the target resource's status so a
/// subsequent Describe reflects the transition instead of the default state.
#[tokio::test]
async fn sagemaker_notebook_instance_stop_start_transitions_status() {
    let server = TestServer::start().await;
    let client = sagemaker_client(&server).await;

    client
        .create_notebook_instance()
        .notebook_instance_name("nb-1")
        .instance_type(aws_sdk_sagemaker::types::InstanceType::MlT2Medium)
        .role_arn("arn:aws:iam::000000000000:role/SageMakerRole")
        .send()
        .await
        .expect("create_notebook_instance");

    client
        .stop_notebook_instance()
        .notebook_instance_name("nb-1")
        .send()
        .await
        .expect("stop_notebook_instance");
    let stopped = client
        .describe_notebook_instance()
        .notebook_instance_name("nb-1")
        .send()
        .await
        .expect("describe after stop");
    assert_eq!(
        stopped.notebook_instance_status(),
        Some(&aws_sdk_sagemaker::types::NotebookInstanceStatus::Stopped),
        "StopNotebookInstance must move status to Stopped"
    );

    client
        .start_notebook_instance()
        .notebook_instance_name("nb-1")
        .send()
        .await
        .expect("start_notebook_instance");
    let started = client
        .describe_notebook_instance()
        .notebook_instance_name("nb-1")
        .send()
        .await
        .expect("describe after start");
    assert_eq!(
        started.notebook_instance_status(),
        Some(&aws_sdk_sagemaker::types::NotebookInstanceStatus::InService),
        "StartNotebookInstance must move status to InService"
    );
}

/// `AddAssociation` is the only creation path for a lineage edge; the edge must
/// then be listed and deletable.
#[tokio::test]
async fn sagemaker_association_round_trip() {
    let server = TestServer::start().await;
    let client = sagemaker_client(&server).await;

    let src = "arn:aws:sagemaker:us-east-1:000000000000:experiment/exp";
    let dst = "arn:aws:sagemaker:us-east-1:000000000000:artifact/art";
    client
        .add_association()
        .source_arn(src)
        .destination_arn(dst)
        .send()
        .await
        .expect("add_association");

    let listed = client
        .list_associations()
        .send()
        .await
        .expect("list_associations");
    assert!(
        listed
            .association_summaries()
            .iter()
            .any(|a| a.source_arn() == Some(src) && a.destination_arn() == Some(dst)),
        "added association should appear in the summaries"
    );

    // Delete removes exactly that edge.
    client
        .delete_association()
        .source_arn(src)
        .destination_arn(dst)
        .send()
        .await
        .expect("delete_association");
    let listed = client
        .list_associations()
        .send()
        .await
        .expect("list_associations after delete");
    assert!(
        !listed
            .association_summaries()
            .iter()
            .any(|a| a.source_arn() == Some(src)),
        "deleted association should be gone"
    );
}

/// A List operation's `NameContains` filter must narrow the result to matching
/// records rather than returning the whole family.
#[tokio::test]
async fn sagemaker_list_name_contains_filter() {
    let server = TestServer::start().await;
    let client = sagemaker_client(&server).await;
    let role = "arn:aws:iam::000000000000:role/r";

    for name in ["alpha-model", "beta-model"] {
        client
            .create_model()
            .model_name(name)
            .execution_role_arn(role)
            .send()
            .await
            .expect("create_model");
    }

    let listed = client
        .list_models()
        .name_contains("alpha")
        .send()
        .await
        .expect("list_models with NameContains");
    let names: Vec<&str> = listed
        .models()
        .iter()
        .filter_map(|m| m.model_name())
        .collect();
    assert!(
        names.contains(&"alpha-model"),
        "filtered list should contain alpha-model, got {names:?}"
    );
    assert!(
        !names.contains(&"beta-model"),
        "NameContains=alpha should exclude beta-model, got {names:?}"
    );
}

// HyperPod cluster node-set management: BatchAddClusterNodes persists nodes that
// ListClusterNodes reflects, BatchReplaceClusterNodes swaps the underlying
// instance, and BatchDeleteClusterNodes removes them.
#[tokio::test]
async fn sagemaker_batch_cluster_nodes_round_trip() {
    use aws_sdk_sagemaker::types::AddClusterNodeSpecification;

    let server = TestServer::start().await;
    let client = sagemaker_client(&server).await;

    client
        .batch_add_cluster_nodes()
        .cluster_name("hp-cluster")
        .nodes_to_add(
            AddClusterNodeSpecification::builder()
                .instance_group_name("workers")
                .increment_target_count_by(2)
                .build()
                .unwrap(),
        )
        .send()
        .await
        .expect("batch_add_cluster_nodes");

    let listed = client
        .list_cluster_nodes()
        .cluster_name("hp-cluster")
        .send()
        .await
        .expect("list_cluster_nodes");
    let nodes = listed.cluster_node_summaries();
    assert_eq!(nodes.len(), 2, "two nodes should be listed after add");
    let node_id = nodes[0]
        .node_logical_id()
        .expect("node_logical_id")
        .to_string();
    let old_instance = nodes[0].instance_id().expect("instance_id").to_string();

    // Replace swaps the underlying instance, keeping the logical id.
    client
        .batch_replace_cluster_nodes()
        .cluster_name("hp-cluster")
        .node_logical_ids(node_id.clone())
        .send()
        .await
        .expect("batch_replace_cluster_nodes");
    let listed = client
        .list_cluster_nodes()
        .cluster_name("hp-cluster")
        .send()
        .await
        .expect("list_cluster_nodes");
    let replaced = listed
        .cluster_node_summaries()
        .iter()
        .find(|n| n.node_logical_id() == Some(node_id.as_str()))
        .expect("replaced node still present");
    assert_ne!(
        replaced.instance_id(),
        Some(old_instance.as_str()),
        "replace should mint a new InstanceId"
    );

    // Delete removes the node; the set shrinks to one.
    client
        .batch_delete_cluster_nodes()
        .cluster_name("hp-cluster")
        .node_logical_ids(node_id)
        .send()
        .await
        .expect("batch_delete_cluster_nodes");
    let listed = client
        .list_cluster_nodes()
        .cluster_name("hp-cluster")
        .send()
        .await
        .expect("list_cluster_nodes");
    assert_eq!(listed.cluster_node_summaries().len(), 1);
}

// AttachClusterNodeNetworkInterface attaches an ENI to a node added by
// BatchAddClusterNodes. aws-sdk-sagemaker predates the operation, so it is
// driven over the raw awsJson1.1 wire.
#[tokio::test]
async fn sagemaker_attach_cluster_node_network_interface() {
    use aws_sdk_sagemaker::types::AddClusterNodeSpecification;

    let server = TestServer::start().await;
    let client = sagemaker_client(&server).await;

    client
        .batch_add_cluster_nodes()
        .cluster_name("hp-eni")
        .nodes_to_add(
            AddClusterNodeSpecification::builder()
                .instance_group_name("workers")
                .increment_target_count_by(1)
                .build()
                .unwrap(),
        )
        .send()
        .await
        .expect("batch_add_cluster_nodes");
    let listed = client
        .list_cluster_nodes()
        .cluster_name("hp-eni")
        .send()
        .await
        .expect("list_cluster_nodes");
    let instance_id = listed.cluster_node_summaries()[0]
        .instance_id()
        .expect("instance_id")
        .to_string();

    let http = reqwest::Client::new();
    let attach = |node: String, eni: &'static str| {
        let req = http
            .post(server.endpoint())
            .header("content-type", "application/x-amz-json-1.1")
            .header(
                "x-amz-target",
                "SageMaker.AttachClusterNodeNetworkInterface",
            )
            .header(
                "authorization",
                "AWS4-HMAC-SHA256 Credential=test/20260101/us-east-1/sagemaker/aws4_request, \
                 SignedHeaders=host, Signature=0",
            )
            .body(
                serde_json::json!({
                    "ClusterName": "hp-eni",
                    "NodeId": node,
                    "NetworkInterfaceId": eni,
                })
                .to_string(),
            );
        async move {
            let resp = req.send().await.expect("request sent");
            let status = resp.status();
            let body: serde_json::Value = resp.json().await.expect("json body");
            (status, body)
        }
    };

    let (status, body) = attach(instance_id.clone(), "eni-0123456789abcdef0").await;
    assert_eq!(status, 200, "attach failed: {body}");
    assert_eq!(body["NodeId"], instance_id.as_str());
    assert_eq!(body["NetworkInterfaceId"], "eni-0123456789abcdef0");
    assert!(body["ClusterArn"].as_str().unwrap().contains(":cluster/"));
    let attachment_id = body["AttachmentId"].as_str().unwrap().to_string();
    assert!(attachment_id.starts_with("eni-attach-"), "{attachment_id}");

    // Re-attaching the same ENI to the same node returns the same attachment.
    let (status, again) = attach(instance_id, "eni-0123456789abcdef0").await;
    assert_eq!(status, 200, "re-attach failed: {again}");
    assert_eq!(again["AttachmentId"], attachment_id.as_str());

    // An unknown node is ResourceNotFound.
    let (status, missing) =
        attach("i-0ffffffffffffffff".to_string(), "eni-0aaaaaaaaaaaaaaaa").await;
    assert_eq!(status, 404, "{missing}");
    assert!(
        missing.to_string().contains("ResourceNotFound"),
        "{missing}"
    );
}

// AssociateTrialComponent makes a component visible under a scoped
// ListTrialComponents(TrialName=…); DisassociateTrialComponent removes it.
#[tokio::test]
async fn sagemaker_trial_component_association_round_trip() {
    let server = TestServer::start().await;
    let client = sagemaker_client(&server).await;

    client
        .create_trial()
        .trial_name("t1")
        .experiment_name("e1")
        .send()
        .await
        .expect("create_trial");
    client
        .create_trial_component()
        .trial_component_name("tc1")
        .send()
        .await
        .expect("create_trial_component");

    client
        .associate_trial_component()
        .trial_component_name("tc1")
        .trial_name("t1")
        .send()
        .await
        .expect("associate_trial_component");

    let scoped = client
        .list_trial_components()
        .trial_name("t1")
        .send()
        .await
        .expect("list_trial_components scoped");
    let names: Vec<&str> = scoped
        .trial_component_summaries()
        .iter()
        .filter_map(|c| c.trial_component_name())
        .collect();
    assert_eq!(
        names,
        vec!["tc1"],
        "scoped list should return the associated component"
    );

    client
        .disassociate_trial_component()
        .trial_component_name("tc1")
        .trial_name("t1")
        .send()
        .await
        .expect("disassociate_trial_component");
    let scoped = client
        .list_trial_components()
        .trial_name("t1")
        .send()
        .await
        .expect("list_trial_components scoped");
    assert!(
        scoped.trial_component_summaries().is_empty(),
        "component should no longer be associated with the trial"
    );
}

// SendPipelineExecutionStepSuccess resolves a waiting callback step and echoes
// the pipeline execution ARN.
#[tokio::test]
async fn sagemaker_send_pipeline_step_success_returns_execution_arn() {
    let server = TestServer::start().await;
    let client = sagemaker_client(&server).await;

    let out = client
        .send_pipeline_execution_step_success()
        .callback_token("cbtoken001")
        .send()
        .await
        .expect("send_pipeline_execution_step_success");
    assert!(
        out.pipeline_execution_arn()
            .unwrap_or_default()
            .contains(":pipeline/"),
        "expected a pipeline execution arn, got {:?}",
        out.pipeline_execution_arn()
    );
}

/// A tagged Create is visible to ListTags; AddTags / DeleteTags are visible to
/// DescribeLabelingJob's `Tags`; delete + same-name re-create starts untagged.
#[tokio::test]
async fn sagemaker_create_time_tags_share_list_tags_store() {
    use aws_sdk_sagemaker::types::{
        HumanTaskConfig, LabelingJobDataSource, LabelingJobInputConfig, LabelingJobOutputConfig,
        LabelingJobS3DataSource, UiConfig,
    };
    let server = TestServer::start().await;
    let client = sagemaker_client(&server).await;
    let role = "arn:aws:iam::000000000000:role/SageMakerRole";

    let created = client
        .create_model()
        .model_name("tagged-model")
        .execution_role_arn(role)
        .tags(Tag::builder().key("env").value("prod").build())
        .send()
        .await
        .expect("create_model");
    let model_arn = created.model_arn().unwrap().to_string();
    let tags = client
        .list_tags()
        .resource_arn(&model_arn)
        .send()
        .await
        .expect("list_tags");
    let tags = tags.tags();
    assert_eq!(tags.len(), 1, "create-time tags missing: {tags:?}");
    assert_eq!(tags[0].key(), Some("env"));
    assert_eq!(tags[0].value(), Some("prod"));

    client
        .delete_model()
        .model_name("tagged-model")
        .send()
        .await
        .expect("delete_model");
    client
        .create_model()
        .model_name("tagged-model")
        .execution_role_arn(role)
        .send()
        .await
        .expect("re-create model");
    let tags = client
        .list_tags()
        .resource_arn(&model_arn)
        .send()
        .await
        .expect("list_tags after re-create");
    assert!(tags.tags().is_empty(), "re-create inherited tags");

    // Describe reflects AddTags / DeleteTags.
    let lj = client
        .create_labeling_job()
        .labeling_job_name("lj")
        .label_attribute_name("label")
        .role_arn(role)
        .input_config(
            LabelingJobInputConfig::builder()
                .data_source(
                    LabelingJobDataSource::builder()
                        .s3_data_source(
                            LabelingJobS3DataSource::builder()
                                .manifest_s3_uri("s3://b/manifest")
                                .build(),
                        )
                        .build(),
                )
                .build(),
        )
        .output_config(
            LabelingJobOutputConfig::builder()
                .s3_output_path("s3://b/out")
                .build(),
        )
        .human_task_config(
            HumanTaskConfig::builder()
                .workteam_arn("arn:aws:sagemaker:us-east-1:000000000000:workteam/private-crowd/w")
                .ui_config(
                    UiConfig::builder()
                        .ui_template_s3_uri("s3://b/t.html")
                        .build(),
                )
                .pre_human_task_lambda_arn("arn:aws:lambda:us-east-1:000000000000:function:pre")
                .task_title("t")
                .task_description("d")
                .number_of_human_workers_per_data_object(1)
                .task_time_limit_in_seconds(60)
                .build(),
        )
        .tags(Tag::builder().key("a").value("1").build())
        .send()
        .await
        .expect("create_labeling_job");
    let lj_arn = lj.labeling_job_arn().expect("labeling_job_arn").to_string();
    client
        .add_tags()
        .resource_arn(&lj_arn)
        .tags(Tag::builder().key("b").value("2").build())
        .send()
        .await
        .expect("add_tags");
    client
        .delete_tags()
        .resource_arn(&lj_arn)
        .tag_keys("a")
        .send()
        .await
        .expect("delete_tags");
    let described = client
        .describe_labeling_job()
        .labeling_job_name("lj")
        .send()
        .await
        .expect("describe_labeling_job");
    let keys: Vec<&str> = described.tags().iter().filter_map(|t| t.key()).collect();
    assert_eq!(keys, vec!["b"]);
}

/// Search runs over real stored resources; Stop on a missing job is
/// ResourceNotFound.
#[tokio::test]
async fn sagemaker_search_and_missing_lifecycle_target() {
    use aws_sdk_sagemaker::types::{Filter, Operator, ResourceType, SearchExpression};
    let server = TestServer::start().await;
    let client = sagemaker_client(&server).await;
    for (name, env) in [("m-prod", "prod"), ("m-dev", "dev")] {
        client
            .create_model()
            .model_name(name)
            .execution_role_arn("arn:aws:iam::000000000000:role/r")
            .tags(Tag::builder().key("env").value(env).build())
            .send()
            .await
            .expect("create_model");
    }
    let out = client
        .search()
        .resource(ResourceType::Model)
        .search_expression(
            SearchExpression::builder()
                .filters(
                    Filter::builder()
                        .name("Tags.env")
                        .operator(Operator::Equals)
                        .value("prod")
                        .build(),
                )
                .build(),
        )
        .send()
        .await
        .expect("search");
    let names: Vec<&str> = out
        .results()
        .iter()
        .filter_map(|r| r.model())
        .filter_map(|m| m.model())
        .filter_map(|m| m.model_name())
        .collect();
    assert_eq!(names, vec!["m-prod"]);

    let err = client
        .stop_training_job()
        .training_job_name("never-created")
        .send()
        .await
        .expect_err("stop on missing job must fail");
    assert_eq!(
        err.into_service_error().meta().code(),
        Some("ResourceNotFound")
    );

    // StopNotebookInstance declares no ResourceNotFound: SageMaker answers
    // ValidationException "RecordNotFound".
    let err = client
        .stop_notebook_instance()
        .notebook_instance_name("never-created")
        .send()
        .await
        .expect_err("stop on missing notebook must fail");
    let err = err.into_service_error();
    assert_eq!(err.meta().code(), Some("ValidationException"));
    assert_eq!(err.meta().message(), Some("RecordNotFound"));
}

/// Reads report resources in their *output* shapes, not the request shapes
/// they were created from: a workforce never echoes its OIDC client secret
/// and is reported inside its `Workforce` structure, a human task UI reports
/// its template's content hash, an inference component reports the image it
/// deployed and its copy counts, and a partial `GitConfig` update keeps the
/// repository URL.
#[tokio::test]
async fn sagemaker_reads_report_output_shapes() {
    use aws_sdk_sagemaker::types::{
        GitConfig, GitConfigForUpdate, InferenceComponentContainerSpecification,
        InferenceComponentRuntimeConfig, InferenceComponentSpecification, OidcConfig, UiTemplate,
    };

    let server = TestServer::start().await;
    let client = sagemaker_client(&server).await;

    client
        .create_workforce()
        .workforce_name("wf")
        .oidc_config(
            OidcConfig::builder()
                .client_id("cid")
                .client_secret("s3cret")
                .issuer("https://issuer.example.com")
                .authorization_endpoint("https://issuer.example.com/auth")
                .token_endpoint("https://issuer.example.com/token")
                .user_info_endpoint("https://issuer.example.com/userinfo")
                .logout_endpoint("https://issuer.example.com/logout")
                .jwks_uri("https://issuer.example.com/jwks")
                .build(),
        )
        .send()
        .await
        .expect("create_workforce");
    let wf = client
        .describe_workforce()
        .workforce_name("wf")
        .send()
        .await
        .expect("describe_workforce");
    let wf = wf.workforce().expect("workforce");
    assert_eq!(wf.workforce_name(), Some("wf"));
    assert_eq!(wf.oidc_config().and_then(|o| o.client_id()), Some("cid"));
    assert!(wf.create_date().is_some());
    let listed = client
        .list_workforces()
        .send()
        .await
        .expect("list_workforces");
    assert_eq!(listed.workforces().len(), 1);
    assert_eq!(
        listed.workforces()[0]
            .oidc_config()
            .and_then(|o| o.issuer()),
        Some("https://issuer.example.com")
    );

    client
        .create_human_task_ui()
        .human_task_ui_name("ui")
        .ui_template(UiTemplate::builder().content("<p>hi</p>").build())
        .send()
        .await
        .expect("create_human_task_ui");
    let ui = client
        .describe_human_task_ui()
        .human_task_ui_name("ui")
        .send()
        .await
        .expect("describe_human_task_ui");
    assert_eq!(
        ui.ui_template().and_then(|t| t.content_sha256()),
        // sha256("<p>hi</p>")
        Some("0a4735281db700223af63abc387c351f64ea6961a1ef955631df08d96169e772")
    );

    let image = "123456789012.dkr.ecr.us-east-1.amazonaws.com/img@sha256:abcd";
    client
        .create_inference_component()
        .inference_component_name("ic")
        .endpoint_name("ep")
        .variant_name("v")
        .specification(
            InferenceComponentSpecification::builder()
                .container(
                    InferenceComponentContainerSpecification::builder()
                        .image(image)
                        .build(),
                )
                .build(),
        )
        .runtime_config(
            InferenceComponentRuntimeConfig::builder()
                .copy_count(2)
                .build(),
        )
        .send()
        .await
        .expect("create_inference_component");
    let ic = client
        .describe_inference_component()
        .inference_component_name("ic")
        .send()
        .await
        .expect("describe_inference_component");
    let deployed = ic
        .specification()
        .and_then(|s| s.container())
        .and_then(|c| c.deployed_image())
        .expect("deployed image");
    assert_eq!(deployed.specified_image(), Some(image));
    assert_eq!(deployed.resolved_image(), Some(image));
    let rc = ic.runtime_config().expect("runtime config");
    assert_eq!(rc.desired_copy_count(), Some(2));
    assert_eq!(rc.current_copy_count(), Some(2));

    client
        .create_code_repository()
        .code_repository_name("repo")
        .git_config(
            GitConfig::builder()
                .repository_url("https://github.com/o/r.git")
                .branch("main")
                .build(),
        )
        .send()
        .await
        .expect("create_code_repository");
    client
        .update_code_repository()
        .code_repository_name("repo")
        .git_config(
            GitConfigForUpdate::builder()
                .secret_arn("arn:aws:secretsmanager:us-east-1:000000000000:secret:s")
                .build(),
        )
        .send()
        .await
        .expect("update_code_repository");
    let repo = client
        .describe_code_repository()
        .code_repository_name("repo")
        .send()
        .await
        .expect("describe_code_repository");
    let git = repo.git_config().expect("git config");
    assert_eq!(git.repository_url(), Some("https://github.com/o/r.git"));
    assert_eq!(git.branch(), Some("main"));
    assert_eq!(
        git.secret_arn(),
        Some("arn:aws:secretsmanager:us-east-1:000000000000:secret:s")
    );
}

#[tokio::test]
async fn sagemaker_model_card_export_job_requires_existing_card() {
    use aws_sdk_sagemaker::types::{ModelCardExportOutputConfig, ModelCardStatus};
    let server = TestServer::start().await;
    let client = sagemaker_client(&server).await;
    let output = ModelCardExportOutputConfig::builder()
        .s3_output_path("s3://bucket/out")
        .build();

    let err = client
        .create_model_card_export_job()
        .model_card_name("no-such-card")
        .model_card_export_job_name("job1")
        .output_config(output.clone())
        .send()
        .await
        .expect_err("exporting a missing model card must fail");
    assert!(
        err.as_service_error()
            .is_some_and(|e| e.is_resource_not_found()),
        "{err:?}"
    );

    client
        .create_model_card()
        .model_card_name("card1")
        .content("{}")
        .model_card_status(ModelCardStatus::Draft)
        .send()
        .await
        .unwrap();
    let job = client
        .create_model_card_export_job()
        .model_card_name("card1")
        .model_card_export_job_name("job2")
        .output_config(output)
        .send()
        .await
        .unwrap();
    assert!(job.model_card_export_job_arn().is_some());
}

#[tokio::test]
async fn sagemaker_workforce_vpc_config_must_be_complete() {
    use aws_sdk_sagemaker::error::ProvideErrorMetadata;
    use aws_sdk_sagemaker::types::WorkforceVpcConfigRequest;
    let server = TestServer::start().await;
    let client = sagemaker_client(&server).await;

    let err = client
        .create_workforce()
        .workforce_name("wf-partial")
        .workforce_vpc_config(WorkforceVpcConfigRequest::builder().vpc_id("vpc-1").build())
        .send()
        .await
        .expect_err("a VPC config without subnets / security groups must be rejected");
    assert_eq!(
        err.as_service_error().and_then(|e| e.code()),
        Some("ValidationException")
    );

    client
        .create_workforce()
        .workforce_name("wf-vpc")
        .workforce_vpc_config(
            WorkforceVpcConfigRequest::builder()
                .vpc_id("vpc-1")
                .security_group_ids("sg-1")
                .subnets("subnet-1")
                .build(),
        )
        .send()
        .await
        .unwrap();
    let listed = client.list_workforces().send().await.unwrap();
    let wf = listed
        .workforces()
        .iter()
        .find(|w| w.workforce_name() == Some("wf-vpc"))
        .expect("workforce listed");
    let vpc = wf.workforce_vpc_config().expect("vpc config");
    assert_eq!(vpc.vpc_id(), Some("vpc-1"));
    assert_eq!(vpc.security_group_ids(), ["sg-1"]);
    assert_eq!(vpc.subnets(), ["subnet-1"]);

    // DescribeWorkforce returns the stored view under its `Workforce` wrapper.
    let described = client
        .describe_workforce()
        .workforce_name("wf-vpc")
        .send()
        .await
        .unwrap();
    let wf = described.workforce().expect("workforce");
    assert_eq!(wf.workforce_name(), Some("wf-vpc"));
    let vpc = wf.workforce_vpc_config().expect("vpc config");
    assert_eq!(vpc.vpc_id(), Some("vpc-1"));
    assert_eq!(vpc.subnets(), ["subnet-1"]);
}
