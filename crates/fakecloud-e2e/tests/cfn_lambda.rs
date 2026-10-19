//! CloudFormation provisioner for `AWS::Lambda::Function`. Covers the
//! three Code source variants (`ZipFile` inline, `S3Bucket` + `S3Key`,
//! `ImageUri`) plus an in-place stack update path that mutates Handler
//! and Environment.Variables on the live function.

mod helpers;

use aws_sdk_cloudformation::types::{Capability, ChangeSetType, OnFailure, Parameter};
use aws_sdk_s3::primitives::ByteStream;
use helpers::TestServer;

const ROLE_TEMPLATE_FRAGMENT: &str = r#"
    "Role": {
      "Type": "AWS::IAM::Role",
      "Properties": {
        "RoleName": "$ROLE_NAME$",
        "AssumeRolePolicyDocument": {
          "Version": "2012-10-17",
          "Statement": [{
            "Effect": "Allow",
            "Principal": {"Service": "lambda.amazonaws.com"},
            "Action": "sts:AssumeRole"
          }]
        }
      }
    }
"#;

fn template_zipfile_inline(role_name: &str) -> String {
    format!(
        r#"{{
  "AWSTemplateFormatVersion": "2010-09-09",
  "Resources": {{
    {role}
    ,
    "Func": {{
      "Type": "AWS::Lambda::Function",
      "Properties": {{
        "FunctionName": "cfn-lambda-zip-fn",
        "Runtime": "nodejs20.x",
        "Handler": "index.handler",
        "Role": {{"Fn::GetAtt": ["Role", "Arn"]}},
        "Code": {{"ZipFile": "exports.handler = async () => ({{ ok: true }});"}},
        "Description": "Created via CFN ZipFile",
        "Timeout": 7,
        "MemorySize": 256,
        "Architectures": ["arm64"],
        "Environment": {{"Variables": {{"FOO": "bar", "BAZ": "qux"}}}},
        "Tags": [
          {{"Key": "App", "Value": "demo"}},
          {{"Key": "Stage", "Value": "test"}}
        ],
        "TracingConfig": {{"Mode": "Active"}},
        "EphemeralStorage": {{"Size": 1024}}
      }}
    }}
  }},
  "Outputs": {{
    "FuncArn":  {{"Value": {{"Fn::GetAtt": ["Func", "Arn"]}}}},
    "FuncName": {{"Value": {{"Ref": "Func"}}}},
    "FuncVersion": {{"Value": {{"Fn::GetAtt": ["Func", "Version"]}}}}
  }}
}}"#,
        role = ROLE_TEMPLATE_FRAGMENT.replace("$ROLE_NAME$", role_name)
    )
}

fn template_zipfile_inline_v2(role_name: &str) -> String {
    // Same as v1 but with a different Handler + an extra env var. Drives
    // the in-place UpdateStack -> update_function_configuration path.
    format!(
        r#"{{
  "AWSTemplateFormatVersion": "2010-09-09",
  "Resources": {{
    {role}
    ,
    "Func": {{
      "Type": "AWS::Lambda::Function",
      "Properties": {{
        "FunctionName": "cfn-lambda-zip-fn",
        "Runtime": "nodejs20.x",
        "Handler": "updated.handler",
        "Role": {{"Fn::GetAtt": ["Role", "Arn"]}},
        "Code": {{"ZipFile": "exports.handler = async () => ({{ ok: true, v: 2 }});"}},
        "Description": "Updated via CFN UpdateStack",
        "Timeout": 13,
        "MemorySize": 512,
        "Architectures": ["arm64"],
        "Environment": {{"Variables": {{"FOO": "bar2", "NEW": "yes"}}}},
        "TracingConfig": {{"Mode": "PassThrough"}}
      }}
    }}
  }}
}}"#,
        role = ROLE_TEMPLATE_FRAGMENT.replace("$ROLE_NAME$", role_name)
    )
}

fn template_s3_code(role_name: &str) -> String {
    // Bucket and key flow in via stack parameters at create time so the
    // test can inject whatever physical names it set up via the S3 SDK.
    format!(
        r#"{{
  "AWSTemplateFormatVersion": "2010-09-09",
  "Parameters": {{
    "CodeBucket": {{"Type": "String"}},
    "CodeKey":    {{"Type": "String"}}
  }},
  "Resources": {{
    {role}
    ,
    "Func": {{
      "Type": "AWS::Lambda::Function",
      "Properties": {{
        "FunctionName": "cfn-lambda-s3-fn",
        "Runtime": "python3.12",
        "Handler": "main.handler",
        "Role": {{"Fn::GetAtt": ["Role", "Arn"]}},
        "Code": {{
          "S3Bucket": {{"Ref": "CodeBucket"}},
          "S3Key": {{"Ref": "CodeKey"}}
        }}
      }}
    }}
  }},
  "Outputs": {{
    "FuncArn": {{"Value": {{"Fn::GetAtt": ["Func", "Arn"]}}}},
    "FuncName": {{"Value": {{"Ref": "Func"}}}}
  }}
}}"#,
        role = ROLE_TEMPLATE_FRAGMENT.replace("$ROLE_NAME$", role_name),
    )
}

#[tokio::test]
async fn cfn_creates_lambda_function_from_zipfile_inline() {
    let server = TestServer::start().await;
    let cfn = server.cloudformation_client().await;
    let lambda = server.lambda_client().await;

    let template = template_zipfile_inline("cfn-lambda-zip-role");
    cfn.create_stack()
        .stack_name("cfn-lambda-zip")
        .template_body(template)
        .capabilities(Capability::CapabilityNamedIam)
        .on_failure(OnFailure::Rollback)
        .send()
        .await
        .expect("create_stack");

    let described = cfn
        .describe_stacks()
        .stack_name("cfn-lambda-zip")
        .send()
        .await
        .expect("describe_stacks");
    let stack = described.stacks().first().expect("stack present");
    assert_eq!(stack.stack_status().unwrap().as_str(), "CREATE_COMPLETE");

    let mut func_arn = None;
    let mut func_name = None;
    let mut func_version = None;
    for o in stack.outputs() {
        match o.output_key() {
            Some("FuncArn") => func_arn = o.output_value().map(String::from),
            Some("FuncName") => func_name = o.output_value().map(String::from),
            Some("FuncVersion") => func_version = o.output_value().map(String::from),
            _ => {}
        }
    }
    let func_arn = func_arn.expect("FuncArn output");
    let func_name = func_name.expect("FuncName output");
    let func_version = func_version.expect("FuncVersion output");

    // Ref returns FunctionName; GetAtt Arn returns the function ARN; Version
    // resolves to the live `$LATEST` since CFN never publishes a numbered
    // version itself.
    assert_eq!(func_name, "cfn-lambda-zip-fn");
    assert!(
        func_arn.ends_with(":function:cfn-lambda-zip-fn"),
        "expected real ARN, got {func_arn}"
    );
    assert_eq!(func_version, "$LATEST");

    // GetFunction confirms every CFN property landed on the live function.
    let got = lambda
        .get_function()
        .function_name(&func_name)
        .send()
        .await
        .expect("get_function");
    let cfg = got.configuration().expect("configuration");
    assert_eq!(cfg.runtime().map(|r| r.as_str()), Some("nodejs20.x"));
    assert_eq!(cfg.handler(), Some("index.handler"));
    assert_eq!(cfg.timeout(), Some(7));
    assert_eq!(cfg.memory_size(), Some(256));
    assert_eq!(cfg.description(), Some("Created via CFN ZipFile"));
    let env = cfg.environment().expect("env");
    let vars = env.variables().expect("vars");
    assert_eq!(vars.get("FOO").map(|s| s.as_str()), Some("bar"));
    assert_eq!(vars.get("BAZ").map(|s| s.as_str()), Some("qux"));
    let archs: Vec<&str> = cfg.architectures().iter().map(|a| a.as_str()).collect();
    assert_eq!(archs, vec!["arm64"]);
    assert_eq!(
        cfg.tracing_config()
            .and_then(|t| t.mode())
            .map(|m| m.as_str()),
        Some("Active")
    );
    assert_eq!(cfg.ephemeral_storage().map(|e| e.size()), Some(1024));
    // Code SHA is non-empty because the inline source bytes were hashed.
    assert!(
        !cfg.code_sha256().unwrap_or_default().is_empty(),
        "code_sha256 should be populated from ZipFile bytes"
    );

    cfn.delete_stack()
        .stack_name("cfn-lambda-zip")
        .send()
        .await
        .expect("delete_stack");

    // After delete, GetFunction must 404.
    let after = lambda.get_function().function_name(&func_name).send().await;
    assert!(
        after.is_err(),
        "function should be gone after stack deletion"
    );
}

#[tokio::test]
async fn cfn_creates_lambda_function_from_s3_code() {
    let server = TestServer::start().await;
    let cfn = server.cloudformation_client().await;
    let lambda = server.lambda_client().await;
    let s3 = server.s3_client().await;

    // Upload the Lambda code to S3 first; CFN will resolve Code.S3Bucket
    // + Code.S3Key against the live S3 state via the cross-service hook.
    let bucket = "cfn-lambda-code-bucket";
    let key = "code/main.zip";
    s3.create_bucket()
        .bucket(bucket)
        .send()
        .await
        .expect("create_bucket");
    let code_bytes = b"def handler(event, context): return {'ok': True}".to_vec();
    s3.put_object()
        .bucket(bucket)
        .key(key)
        .body(ByteStream::from(code_bytes.clone()))
        .send()
        .await
        .expect("put_object");

    let template = template_s3_code("cfn-lambda-s3-role");
    cfn.create_stack()
        .stack_name("cfn-lambda-s3")
        .template_body(template)
        .parameters(
            Parameter::builder()
                .parameter_key("CodeBucket")
                .parameter_value(bucket)
                .build(),
        )
        .parameters(
            Parameter::builder()
                .parameter_key("CodeKey")
                .parameter_value(key)
                .build(),
        )
        .capabilities(Capability::CapabilityNamedIam)
        .on_failure(OnFailure::Rollback)
        .send()
        .await
        .expect("create_stack");

    let described = cfn
        .describe_stacks()
        .stack_name("cfn-lambda-s3")
        .send()
        .await
        .expect("describe_stacks");
    let stack = described.stacks().first().expect("stack");
    assert_eq!(stack.stack_status().unwrap().as_str(), "CREATE_COMPLETE");

    let func_name = stack
        .outputs()
        .iter()
        .find(|o| o.output_key() == Some("FuncName"))
        .and_then(|o| o.output_value())
        .expect("FuncName output");

    let got = lambda
        .get_function()
        .function_name(func_name)
        .send()
        .await
        .expect("get_function");
    let cfg = got.configuration().expect("configuration");
    // Code size matches the bytes we uploaded to S3 — proves the
    // cross-service S3 read happened at provision time.
    assert_eq!(cfg.code_size(), code_bytes.len() as i64);
    assert!(!cfg.code_sha256().unwrap_or_default().is_empty());
    assert_eq!(cfg.handler(), Some("main.handler"));
    assert_eq!(cfg.runtime().map(|r| r.as_str()), Some("python3.12"));
}

#[tokio::test]
async fn cfn_update_lambda_function_mutates_handler_and_env() {
    let server = TestServer::start().await;
    let cfn = server.cloudformation_client().await;
    let lambda = server.lambda_client().await;

    let v1 = template_zipfile_inline("cfn-lambda-update-role");
    cfn.create_stack()
        .stack_name("cfn-lambda-update")
        .template_body(v1)
        .capabilities(Capability::CapabilityNamedIam)
        .on_failure(OnFailure::Rollback)
        .send()
        .await
        .expect("create_stack");

    // Sanity check: v1 handler is index.handler, FOO=bar.
    let before = lambda
        .get_function()
        .function_name("cfn-lambda-zip-fn")
        .send()
        .await
        .expect("get_function v1");
    let cfg = before.configuration().expect("cfg v1");
    assert_eq!(cfg.handler(), Some("index.handler"));
    assert_eq!(cfg.timeout(), Some(7));
    let v1_revision = cfg.revision_id().map(String::from);

    let v2 = template_zipfile_inline_v2("cfn-lambda-update-role");
    cfn.update_stack()
        .stack_name("cfn-lambda-update")
        .template_body(v2)
        .capabilities(Capability::CapabilityNamedIam)
        .send()
        .await
        .expect("update_stack");

    let after = lambda
        .get_function()
        .function_name("cfn-lambda-zip-fn")
        .send()
        .await
        .expect("get_function v2");
    let cfg = after.configuration().expect("cfg v2");
    assert_eq!(cfg.handler(), Some("updated.handler"));
    assert_eq!(cfg.timeout(), Some(13));
    assert_eq!(cfg.memory_size(), Some(512));
    assert_eq!(cfg.description(), Some("Updated via CFN UpdateStack"));
    let env = cfg.environment().expect("env v2");
    let vars = env.variables().expect("vars v2");
    assert_eq!(vars.get("FOO").map(|s| s.as_str()), Some("bar2"));
    assert_eq!(vars.get("NEW").map(|s| s.as_str()), Some("yes"));
    // BAZ from v1 is no longer present — env is replaced wholesale.
    assert!(!vars.contains_key("BAZ"));
    assert_eq!(
        cfg.tracing_config()
            .and_then(|t| t.mode())
            .map(|m| m.as_str()),
        Some("PassThrough")
    );
    // RevisionId rotates whenever the configuration mutates.
    assert_ne!(cfg.revision_id().map(String::from), v1_revision);
}

/// Regression: a custom-resource handler that raises still returns 200 with the
/// error in its body, and the provisioner treated that as success — the stack
/// reached CREATE_COMPLETE with the resource having done nothing. Real
/// CloudFormation cannot reach that state: the resource signals FAILED, or never
/// signals and the stack times out.
///
/// The handler code comes from S3 rather than an inline `ZipFile` so the test
/// exercises only the behaviour under test.
#[tokio::test]
async fn cfn_fails_a_stack_whose_custom_resource_handler_raises() {
    if !docker_available() {
        eprintln!("docker required for Lambda execution; skipping");
        return;
    }
    let server = TestServer::start().await;
    let cfn = server.cloudformation_client().await;
    let s3 = server.s3_client().await;

    let bucket = "cfn-custom-raise-code";
    s3.create_bucket()
        .bucket(bucket)
        .send()
        .await
        .expect("create_bucket");
    let zip_bytes = build_python_handler_zip(
        "def handler(event, context):\n    raise RuntimeError('handler blew up')\n",
    );
    s3.put_object()
        .bucket(bucket)
        .key("cr.zip")
        .body(ByteStream::from(zip_bytes))
        .send()
        .await
        .expect("put_object");

    let template = format!(
        r#"{{
  "Resources": {{
    "Handler": {{
      "Type": "AWS::Lambda::Function",
      "Properties": {{
        "FunctionName": "raising-custom-resource",
        "Runtime": "python3.12",
        "Handler": "index.handler",
        "Timeout": 30,
        "Role": "arn:aws:iam::123456789012:role/cr-role",
        "Code": {{"S3Bucket": "{bucket}", "S3Key": "cr.zip"}}
      }}
    }},
    "Custom": {{
      "Type": "Custom::Thing",
      "Properties": {{"ServiceToken": {{"Fn::GetAtt": ["Handler", "Arn"]}}}}
    }}
  }}
}}"#
    );

    cfn.create_stack()
        .stack_name("cfn-custom-raises")
        .template_body(template)
        .capabilities(Capability::CapabilityNamedIam)
        .on_failure(OnFailure::DoNothing)
        .send()
        .await
        .expect("create_stack");

    // The custom-resource invoke is deferred off the request path, so poll for
    // a terminal status rather than reading the in-progress one.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    let (status, reason) = loop {
        let described = cfn
            .describe_stacks()
            .stack_name("cfn-custom-raises")
            .send()
            .await
            .expect("describe_stacks");
        let stack = described.stacks().first().expect("stack").clone();
        let status = stack.stack_status().unwrap().as_str().to_string();
        if !status.ends_with("_IN_PROGRESS") {
            break (
                status,
                stack.stack_status_reason().unwrap_or_default().to_string(),
            );
        }
        assert!(
            std::time::Instant::now() < deadline,
            "stack never reached a terminal status"
        );
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    };

    assert_ne!(
        status, "CREATE_COMPLETE",
        "a raising custom resource must not report success"
    );
    assert!(
        reason.contains("handler blew up") || reason.contains("RuntimeError"),
        "failure reason should name the handler's error, got: {reason}"
    );
}

/// Regression: the same failing handler as above, provisioned through
/// `CreateChangeSet` + `ExecuteChangeSet` -- the path `cdk deploy`,
/// `aws cloudformation deploy` and SAM all take.
///
/// That path queues custom-resource invokes instead of running them
/// (`defer_custom_invokes`), and the queue drain logged the outcome and threw
/// it away, so every fix for reading a custom resource's outcome applied only
/// to `CreateStack`. A handler that raised still produced CREATE_COMPLETE.
#[tokio::test]
async fn cfn_changeset_fails_a_stack_whose_custom_resource_handler_raises() {
    if !docker_available() {
        eprintln!("docker required for Lambda execution; skipping");
        return;
    }
    let server = TestServer::start().await;
    let cfn = server.cloudformation_client().await;
    let s3 = server.s3_client().await;

    let bucket = "cfn-cs-custom-raise-code";
    s3.create_bucket()
        .bucket(bucket)
        .send()
        .await
        .expect("create_bucket");
    s3.put_object()
        .bucket(bucket)
        .key("cr.zip")
        .body(ByteStream::from(build_python_handler_zip(
            "def handler(event, context):\n    raise RuntimeError('changeset handler blew up')\n",
        )))
        .send()
        .await
        .expect("put_object");

    let template = custom_resource_template(bucket, "cs-raising-custom-resource");

    let cs = cfn
        .create_change_set()
        .stack_name("cfn-cs-custom-raises")
        .change_set_name("cs1")
        .change_set_type(ChangeSetType::Create)
        .template_body(template)
        .capabilities(Capability::CapabilityNamedIam)
        .send()
        .await
        .expect("create_change_set");

    let cs_id = cs.id().expect("change set id");
    cfn.execute_change_set()
        .change_set_name(cs_id)
        .send()
        .await
        .expect("execute_change_set");

    let (status, reason) = await_terminal_status(&cfn, "cfn-cs-custom-raises").await;
    assert_ne!(
        status, "CREATE_COMPLETE",
        "a raising custom resource must not report success on the changeset path"
    );
    assert!(
        reason.contains("changeset handler blew up") || reason.contains("RuntimeError"),
        "failure reason should name the handler's error, got: {reason}"
    );
    // The execution failed, and a rolled-back stack publishes nothing: no
    // outputs, and no export another stack could import.
    assert_eq!(
        change_set_execution_status(&cfn, cs_id).await,
        "EXECUTE_FAILED"
    );
    assert!(
        stack_outputs(&cfn, "cfn-cs-custom-raises").await.is_empty(),
        "a failed stack must not report outputs"
    );
    let exports = cfn.list_exports().send().await.expect("list_exports");
    assert!(
        exports
            .exports()
            .iter()
            .all(|e| e.name() != Some("cs-raising-custom-resource-arn")),
        "a failed stack must not export its outputs"
    );
}

/// Regression: `ExecuteChangeSet` wrote the terminal status before the queued
/// custom-resource invokes had run, so the stack read CREATE_COMPLETE while its
/// handlers were still working. `cdk deploy` returns on that status, which is
/// how `BucketDeployment` could report success with the bucket still empty.
///
/// Real CloudFormation holds the stack `CREATE_IN_PROGRESS` until every custom
/// resource signals. The handler sleeps so the window is unambiguous.
#[tokio::test]
async fn cfn_changeset_holds_the_stack_in_progress_until_custom_resources_finish() {
    if !docker_available() {
        eprintln!("docker required for Lambda execution; skipping");
        return;
    }
    let server = TestServer::start().await;
    let cfn = server.cloudformation_client().await;
    let s3 = server.s3_client().await;

    let bucket = "cfn-cs-custom-slow-code";
    s3.create_bucket()
        .bucket(bucket)
        .send()
        .await
        .expect("create_bucket");
    s3.put_object()
        .bucket(bucket)
        .key("cr.zip")
        .body(ByteStream::from(build_python_handler_zip(
            "import time\n\n\ndef handler(event, context):\n    time.sleep(5)\n    return {}\n",
        )))
        .send()
        .await
        .expect("put_object");

    let template = custom_resource_template(bucket, "cs-slow-custom-resource");

    let cs = cfn
        .create_change_set()
        .stack_name("cfn-cs-custom-slow")
        .change_set_name("cs1")
        .change_set_type(ChangeSetType::Create)
        .template_body(template)
        .capabilities(Capability::CapabilityNamedIam)
        .send()
        .await
        .expect("create_change_set");

    let cs_id = cs.id().expect("change set id");
    cfn.execute_change_set()
        .change_set_name(cs_id)
        .send()
        .await
        .expect("execute_change_set");

    // The handler is still sleeping, so the stack cannot legitimately be
    // complete yet. This is the assertion the old fire-and-forget drain failed.
    let status = cfn
        .describe_stacks()
        .stack_name("cfn-cs-custom-slow")
        .send()
        .await
        .expect("describe_stacks")
        .stacks()
        .first()
        .expect("stack")
        .stack_status()
        .expect("status")
        .as_str()
        .to_string();
    assert_eq!(
        status, "CREATE_IN_PROGRESS",
        "stack must stay in progress while its custom resource runs"
    );
    assert_eq!(
        change_set_execution_status(&cfn, cs_id).await,
        "EXECUTE_IN_PROGRESS"
    );

    let (status, reason) = await_terminal_status(&cfn, "cfn-cs-custom-slow").await;
    assert_eq!(status, "CREATE_COMPLETE", "reason: {reason}");
    assert_eq!(
        change_set_execution_status(&cfn, cs_id).await,
        "EXECUTE_COMPLETE"
    );
    let outputs = stack_outputs(&cfn, "cfn-cs-custom-slow").await;
    assert!(
        outputs
            .iter()
            .any(|(k, v)| k == "HandlerArn" && v.ends_with(":function:cs-slow-custom-resource")),
        "a successful stack keeps its outputs, got: {outputs:?}"
    );
}

/// Regression: changing a custom resource's properties sent the handler a
/// Delete and then a Create, never an Update. The Delete also carried no
/// `ResourceProperties`, so CDK's `BucketDeployment` handler failed with
/// "Missing key 'ResourceProperties'" on every redeploy.
///
/// Real CloudFormation sends one Update carrying the current
/// `PhysicalResourceId`, the new `ResourceProperties` and the previous ones as
/// `OldResourceProperties`, and a Delete carries `ResourceProperties` too.
#[tokio::test]
async fn cfn_update_sends_a_custom_resource_an_update_event() {
    if !docker_available() {
        eprintln!("docker required for Lambda execution; skipping");
        return;
    }
    let server = TestServer::start().await;
    let cfn = server.cloudformation_client().await;
    let stack = "cfn-custom-update-event";
    let function = "cr-update-event";
    upload_custom_resource_handler(&server, "cfn-cr-update-event-code", None).await;

    cfn.create_stack()
        .stack_name(stack)
        .template_body(versioned_custom_resource_template(
            "cfn-cr-update-event-code",
            function,
            "1",
        ))
        .send()
        .await
        .expect("create_stack");
    let (status, reason) = await_terminal_status(&cfn, stack).await;
    assert_eq!(status, "CREATE_COMPLETE", "reason: {reason}");
    let physical_id = custom_physical_id(&cfn, stack).await;

    cfn.update_stack()
        .stack_name(stack)
        .template_body(versioned_custom_resource_template(
            "cfn-cr-update-event-code",
            function,
            "2",
        ))
        .send()
        .await
        .expect("update_stack");
    let (status, reason) = await_terminal_status(&cfn, stack).await;
    assert_eq!(status, "UPDATE_COMPLETE", "reason: {reason}");
    assert_eq!(
        custom_physical_id(&cfn, stack).await,
        physical_id,
        "an in-place update keeps the physical id"
    );

    let events = custom_resource_events(&server, function, 2).await;
    let types: Vec<&str> = events
        .iter()
        .map(|e| e["RequestType"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(types, ["Create", "Update"], "events: {events:#?}");
    let update = &events[1];
    assert_eq!(update["PhysicalResourceId"], physical_id.as_str());
    assert_eq!(update["LogicalResourceId"], "Custom");
    assert_eq!(update["ResourceType"], "Custom::Thing");
    assert_eq!(update["ResourceProperties"]["Version"], "2");
    assert_eq!(update["OldResourceProperties"]["Version"], "1");
    assert!(
        update["ServiceToken"]
            .as_str()
            .is_some_and(|t| t.ends_with(&format!(":function:{function}"))),
        "update event: {update:#}"
    );
    assert!(
        update["ResponseURL"].is_string(),
        "update event: {update:#}"
    );

    cfn.delete_stack()
        .stack_name(stack)
        .send()
        .await
        .expect("delete_stack");
    let events = custom_resource_events(&server, function, 3).await;
    let delete = &events[2];
    assert_eq!(delete["RequestType"], "Delete");
    assert_eq!(delete["PhysicalResourceId"], physical_id.as_str());
    assert_eq!(delete["ResourceProperties"]["Version"], "2");
}

/// Regression: `UpdateStack` ran its custom-resource invokes fire-and-forget,
/// so a handler that raised left the stack UPDATE_COMPLETE. Real
/// CloudFormation marks the resource UPDATE_FAILED and rolls the stack back to
/// UPDATE_ROLLBACK_COMPLETE.
#[tokio::test]
async fn cfn_update_rolls_back_when_a_custom_resource_update_fails() {
    if !docker_available() {
        eprintln!("docker required for Lambda execution; skipping");
        return;
    }
    let server = TestServer::start().await;
    let cfn = server.cloudformation_client().await;
    let stack = "cfn-custom-update-fails";
    let bucket = "cfn-cr-update-fails-code";
    upload_custom_resource_handler(&server, bucket, Some("Update")).await;

    cfn.create_stack()
        .stack_name(stack)
        .template_body(versioned_custom_resource_template(
            bucket,
            "cr-update-fails",
            "1",
        ))
        .send()
        .await
        .expect("create_stack");
    let (status, reason) = await_terminal_status(&cfn, stack).await;
    assert_eq!(status, "CREATE_COMPLETE", "reason: {reason}");

    cfn.update_stack()
        .stack_name(stack)
        .template_body(versioned_custom_resource_template(
            bucket,
            "cr-update-fails",
            "2",
        ))
        .send()
        .await
        .expect("update_stack");
    let (status, reason) = await_terminal_status(&cfn, stack).await;
    assert_eq!(status, "UPDATE_ROLLBACK_COMPLETE", "reason: {reason}");
    assert!(
        reason.contains("Update handler blew up"),
        "failure reason should name the handler's error, got: {reason}"
    );
}

/// Regression: `DeleteStack` ran its custom-resource Delete invokes
/// fire-and-forget and reported DELETE_COMPLETE at once, so a handler that
/// raised went unnoticed. It also deleted the handler Lambda before the queued
/// Delete ran, so the handler was never reached. Real CloudFormation deletes
/// the custom resource first and, when its handler fails, leaves the stack
/// DELETE_FAILED, still holding that resource and the handler it depends on.
#[tokio::test]
async fn cfn_delete_fails_when_a_custom_resource_delete_fails() {
    if !docker_available() {
        eprintln!("docker required for Lambda execution; skipping");
        return;
    }
    let server = TestServer::start().await;
    let cfn = server.cloudformation_client().await;
    let stack = "cfn-custom-delete-fails";
    let bucket = "cfn-cr-delete-fails-code";
    upload_custom_resource_handler(&server, bucket, Some("Delete")).await;

    cfn.create_stack()
        .stack_name(stack)
        .template_body(versioned_custom_resource_template(
            bucket,
            "cr-delete-fails",
            "1",
        ))
        .send()
        .await
        .expect("create_stack");
    let (status, reason) = await_terminal_status(&cfn, stack).await;
    assert_eq!(status, "CREATE_COMPLETE", "reason: {reason}");

    cfn.delete_stack()
        .stack_name(stack)
        .send()
        .await
        .expect("delete_stack");

    // A deleted stack no longer answers DescribeStacks by name, so a missing
    // stack reads as DELETE_COMPLETE.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
    let (status, reason) = loop {
        let Ok(described) = cfn.describe_stacks().stack_name(stack).send().await else {
            break ("DELETE_COMPLETE".to_string(), String::new());
        };
        let s = described.stacks().first().expect("stack").clone();
        let status = s.stack_status().unwrap().as_str().to_string();
        if !status.ends_with("_IN_PROGRESS") {
            break (
                status,
                s.stack_status_reason().unwrap_or_default().to_string(),
            );
        }
        assert!(
            std::time::Instant::now() < deadline,
            "stack never reached a terminal status"
        );
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    };
    assert_eq!(status, "DELETE_FAILED", "reason: {reason}");
    assert!(
        reason.contains("Delete handler blew up"),
        "failure reason should name the handler's error, got: {reason}"
    );
    let resources = cfn
        .describe_stack_resources()
        .stack_name(stack)
        .send()
        .await
        .expect("describe_stack_resources");
    let mut logical_ids: Vec<&str> = resources
        .stack_resources()
        .iter()
        .filter_map(|r| r.logical_resource_id())
        .collect();
    logical_ids.sort_unstable();
    assert_eq!(
        logical_ids,
        ["Custom", "Handler"],
        "the resource that failed to delete, and the handler it depends on, remain"
    );
}

/// Regression: a custom resource's response `Data` was never stored, so
/// `Fn::GetAtt [Custom, Key]` resolved to the placeholder "Custom.Key". CDK's
/// cross-region references depend on it: `Custom::CrossRegionExportReader`
/// returns the values in `Data` and the consumer reads them with
/// `Fn::GetAtt [ExportsReader, "/cdk/exports/<stack>/<name>"]`, so a
/// CloudFront distribution got the placeholder as its certificate ARN.
///
/// Real CloudFormation waits for the handler's response before provisioning a
/// dependent, and serves its `Data` through `Fn::GetAtt` -- to resources and
/// Outputs alike, and again after an Update.
#[tokio::test]
async fn cfn_create_and_update_stack_resolve_getatt_from_custom_resource_data() {
    if !docker_available() {
        eprintln!("docker required for Lambda execution; skipping");
        return;
    }
    let server = TestServer::start().await;
    let cfn = server.cloudformation_client().await;
    let stack = "cfn-custom-data";
    let bucket = "cfn-custom-data-code";
    upload_data_handler(&server, bucket).await;

    cfn.create_stack()
        .stack_name(stack)
        .template_body(custom_data_template(bucket, "cr-data", "put", "1"))
        .send()
        .await
        .expect("create_stack");
    let (status, reason) = await_terminal_status(&cfn, stack).await;
    assert_eq!(status, "CREATE_COMPLETE", "reason: {reason}");
    assert_custom_data_resolved(&server, stack, "cr-data", "1").await;

    cfn.update_stack()
        .stack_name(stack)
        .template_body(custom_data_template(bucket, "cr-data", "put", "2"))
        .send()
        .await
        .expect("update_stack");
    let (status, reason) = await_terminal_status(&cfn, stack).await;
    assert_eq!(status, "UPDATE_COMPLETE", "reason: {reason}");
    assert_custom_data_resolved(&server, stack, "cr-data", "2").await;
}

/// The same as above through `CreateChangeSet` + `ExecuteChangeSet`, the path
/// `cdk deploy` takes. That path queued custom-resource invokes to run after
/// every resource was provisioned, so a dependent could never see the `Data`.
#[tokio::test]
async fn cfn_changeset_resolves_getatt_from_custom_resource_data() {
    if !docker_available() {
        eprintln!("docker required for Lambda execution; skipping");
        return;
    }
    let server = TestServer::start().await;
    let cfn = server.cloudformation_client().await;
    let stack = "cfn-cs-custom-data";
    let bucket = "cfn-cs-custom-data-code";
    upload_data_handler(&server, bucket).await;

    for (change_set_type, version) in [(ChangeSetType::Create, "1"), (ChangeSetType::Update, "2")] {
        let cs = cfn
            .create_change_set()
            .stack_name(stack)
            .change_set_name(format!("cs{version}"))
            .change_set_type(change_set_type)
            .template_body(custom_data_template(bucket, "cs-cr-data", "put", version))
            .send()
            .await
            .expect("create_change_set");
        let cs_id = cs.id().expect("change set id");
        cfn.execute_change_set()
            .change_set_name(cs_id)
            .send()
            .await
            .expect("execute_change_set");
        let (status, reason) = await_terminal_status(&cfn, stack).await;
        assert!(status.ends_with("_COMPLETE"), "{status}: {reason}");
        assert!(!status.contains("ROLLBACK"), "{status}: {reason}");
        assert_eq!(
            change_set_execution_status(&cfn, cs_id).await,
            "EXECUTE_COMPLETE"
        );
        assert_custom_data_resolved(&server, stack, "cs-cr-data", version).await;
    }
}

/// A handler that returns the response body instead of PUTting it to the
/// `ResponseURL`. fakecloud already honours a returned `{"Status":"FAILED"}`;
/// a returned `Data` is honoured the same way.
#[tokio::test]
async fn cfn_changeset_resolves_getatt_from_a_returned_custom_resource_body() {
    if !docker_available() {
        eprintln!("docker required for Lambda execution; skipping");
        return;
    }
    let server = TestServer::start().await;
    let cfn = server.cloudformation_client().await;
    let stack = "cfn-cs-custom-data-return";
    let bucket = "cfn-cs-custom-data-return-code";
    upload_data_handler(&server, bucket).await;

    let cs = cfn
        .create_change_set()
        .stack_name(stack)
        .change_set_name("cs1")
        .change_set_type(ChangeSetType::Create)
        .template_body(custom_data_template(
            bucket,
            "cs-cr-data-return",
            "return",
            "1",
        ))
        .send()
        .await
        .expect("create_change_set");
    cfn.execute_change_set()
        .change_set_name(cs.id().expect("change set id"))
        .send()
        .await
        .expect("execute_change_set");
    let (status, reason) = await_terminal_status(&cfn, stack).await;
    assert_eq!(status, "CREATE_COMPLETE", "reason: {reason}");
    assert_custom_data_resolved(&server, stack, "cs-cr-data-return", "1").await;
}

/// Upload a handler that answers every event with `Data` derived from its
/// `Version` property: PUT to the `ResponseURL` as `cfn-response` does, or
/// returned as the invoke result when its `Mode` property is `return`.
async fn upload_data_handler(server: &TestServer, bucket: &str) {
    let s3 = server.s3_client().await;
    s3.create_bucket()
        .bucket(bucket)
        .send()
        .await
        .expect("create_bucket");
    s3.put_object()
        .bucket(bucket)
        .key("cr.zip")
        .body(ByteStream::from(build_python_handler_zip(
            r#"import json
import urllib.request


def handler(event, context):
    props = event['ResourceProperties']
    version = props.get('Version', '')
    response = {
        'Status': 'SUCCESS',
        'RequestId': event['RequestId'],
        'StackId': event['StackId'],
        'LogicalResourceId': event['LogicalResourceId'],
        'PhysicalResourceId': event.get('PhysicalResourceId', 'custom-data'),
        'Data': {
            'Value': 'value-' + version,
            '/cdk/exports/consumer/cert': 'arn:aws:acm:us-east-1:123456789012:certificate/v' + version,
        },
    }
    if props.get('Mode') == 'return':
        return response
    request = urllib.request.Request(
        event['ResponseURL'],
        data=json.dumps(response).encode(),
        method='PUT',
        headers={'Content-Type': ''},
    )
    urllib.request.urlopen(request)
    return {}
"#,
        )))
        .send()
        .await
        .expect("put_object");
}

/// A custom resource answered by `upload_data_handler`'s handler, an SSM
/// parameter whose value is its `Data.Value`, and Outputs reading `Data` by a
/// plain and a slash-containing key, the shape CDK's cross-region reader uses.
fn custom_data_template(bucket: &str, function_name: &str, mode: &str, version: &str) -> String {
    format!(
        r#"{{
  "Resources": {{
    "Handler": {{
      "Type": "AWS::Lambda::Function",
      "Properties": {{
        "FunctionName": "{function_name}",
        "Runtime": "python3.12",
        "Handler": "index.handler",
        "Timeout": 30,
        "Role": "arn:aws:iam::123456789012:role/cr-role",
        "Code": {{"S3Bucket": "{bucket}", "S3Key": "cr.zip"}}
      }}
    }},
    "Custom": {{
      "Type": "Custom::CrossRegionExportReader",
      "Properties": {{
        "ServiceToken": {{"Fn::GetAtt": ["Handler", "Arn"]}},
        "Mode": "{mode}",
        "Version": "{version}"
      }}
    }},
    "Param": {{
      "Type": "AWS::SSM::Parameter",
      "Properties": {{
        "Name": "/{function_name}/value",
        "Type": "String",
        "Value": {{"Fn::GetAtt": ["Custom", "Value"]}}
      }}
    }}
  }},
  "Outputs": {{
    "Value": {{"Value": {{"Fn::GetAtt": ["Custom", "Value"]}}}},
    "Cert": {{"Value": {{"Fn::GetAtt": ["Custom", "/cdk/exports/consumer/cert"]}}}}
  }}
}}"#
    )
}

/// The dependent SSM parameter and both Outputs hold the `Data` the handler
/// answered `version` with.
async fn assert_custom_data_resolved(
    server: &TestServer,
    stack: &str,
    function_name: &str,
    version: &str,
) {
    let ssm = server.ssm_client().await;
    let param = ssm
        .get_parameter()
        .name(format!("/{function_name}/value"))
        .send()
        .await
        .expect("get_parameter");
    assert_eq!(
        param.parameter().and_then(|p| p.value()),
        Some(format!("value-{version}").as_str()),
        "the dependent resource reads the handler's Data"
    );
    let cfn = server.cloudformation_client().await;
    let mut outputs = stack_outputs(&cfn, stack).await;
    outputs.sort_unstable();
    assert_eq!(
        outputs,
        [
            (
                "Cert".to_string(),
                format!("arn:aws:acm:us-east-1:123456789012:certificate/v{version}")
            ),
            ("Value".to_string(), format!("value-{version}")),
        ]
    );
}

/// Upload a handler to `s3://{bucket}/cr.zip` that succeeds, or raises
/// "<RequestType> handler blew up" for the request type `fail_on`.
async fn upload_custom_resource_handler(server: &TestServer, bucket: &str, fail_on: Option<&str>) {
    let s3 = server.s3_client().await;
    s3.create_bucket()
        .bucket(bucket)
        .send()
        .await
        .expect("create_bucket");
    let fail_on = fail_on.unwrap_or("");
    s3.put_object()
        .bucket(bucket)
        .key("cr.zip")
        .body(ByteStream::from(build_python_handler_zip(&format!(
            "def handler(event, context):\n    if event['RequestType'] == '{fail_on}':\n        raise RuntimeError('{fail_on} handler blew up')\n    return {{}}\n"
        ))))
        .send()
        .await
        .expect("put_object");
}

/// A custom resource carrying `Version` as its one property, backed by the
/// handler at `s3://{bucket}/cr.zip`.
fn versioned_custom_resource_template(bucket: &str, function_name: &str, version: &str) -> String {
    format!(
        r#"{{
  "Resources": {{
    "Handler": {{
      "Type": "AWS::Lambda::Function",
      "Properties": {{
        "FunctionName": "{function_name}",
        "Runtime": "python3.12",
        "Handler": "index.handler",
        "Timeout": 30,
        "Role": "arn:aws:iam::123456789012:role/cr-role",
        "Code": {{"S3Bucket": "{bucket}", "S3Key": "cr.zip"}}
      }}
    }},
    "Custom": {{
      "Type": "Custom::Thing",
      "Properties": {{
        "ServiceToken": {{"Fn::GetAtt": ["Handler", "Arn"]}},
        "Version": "{version}"
      }}
    }}
  }}
}}"#
    )
}

/// The `Custom` resource's physical id.
async fn custom_physical_id(cfn: &aws_sdk_cloudformation::Client, stack_name: &str) -> String {
    cfn.describe_stack_resource()
        .stack_name(stack_name)
        .logical_resource_id("Custom")
        .send()
        .await
        .expect("describe_stack_resource")
        .stack_resource_detail()
        .and_then(|r| r.physical_resource_id())
        .expect("physical id")
        .to_string()
}

/// The custom-resource events delivered to `function_name`, oldest first,
/// waiting until at least `count` have arrived.
async fn custom_resource_events(
    server: &TestServer,
    function_name: &str,
    count: usize,
) -> Vec<serde_json::Value> {
    let suffix = format!(":function:{function_name}");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        let body: serde_json::Value = reqwest::get(format!(
            "{}/_fakecloud/lambda/invocations",
            server.endpoint()
        ))
        .await
        .expect("invocations")
        .json()
        .await
        .expect("invocations json");
        let events: Vec<serde_json::Value> = body["invocations"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|i| {
                i["functionArn"]
                    .as_str()
                    .is_some_and(|a| a.ends_with(&suffix))
            })
            .filter_map(|i| serde_json::from_str(i["payload"].as_str()?).ok())
            .collect();
        if events.len() >= count {
            return events;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "expected {count} events, got: {events:#?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
}

/// `ExecutionStatus` of a change set, as DescribeChangeSet reports it.
async fn change_set_execution_status(
    cfn: &aws_sdk_cloudformation::Client,
    change_set_id: &str,
) -> String {
    cfn.describe_change_set()
        .change_set_name(change_set_id)
        .send()
        .await
        .expect("describe_change_set")
        .execution_status()
        .expect("execution status")
        .as_str()
        .to_string()
}

/// A stack's outputs as `(key, value)` pairs.
async fn stack_outputs(
    cfn: &aws_sdk_cloudformation::Client,
    stack_name: &str,
) -> Vec<(String, String)> {
    cfn.describe_stacks()
        .stack_name(stack_name)
        .send()
        .await
        .expect("describe_stacks")
        .stacks()
        .first()
        .expect("stack")
        .outputs()
        .iter()
        .map(|o| {
            (
                o.output_key().unwrap_or_default().to_string(),
                o.output_value().unwrap_or_default().to_string(),
            )
        })
        .collect()
}

/// A Lambda-backed custom resource whose handler code is the object at
/// `s3://{bucket}/cr.zip`, exporting the handler's ARN as `{function_name}-arn`.
fn custom_resource_template(bucket: &str, function_name: &str) -> String {
    format!(
        r#"{{
  "Resources": {{
    "Handler": {{
      "Type": "AWS::Lambda::Function",
      "Properties": {{
        "FunctionName": "{function_name}",
        "Runtime": "python3.12",
        "Handler": "index.handler",
        "Timeout": 30,
        "Role": "arn:aws:iam::123456789012:role/cr-role",
        "Code": {{"S3Bucket": "{bucket}", "S3Key": "cr.zip"}}
      }}
    }},
    "Custom": {{
      "Type": "Custom::Thing",
      "Properties": {{"ServiceToken": {{"Fn::GetAtt": ["Handler", "Arn"]}}}}
    }}
  }},
  "Outputs": {{
    "HandlerArn": {{
      "Value": {{"Fn::GetAtt": ["Handler", "Arn"]}},
      "Export": {{"Name": "{function_name}-arn"}}
    }}
  }}
}}"#
    )
}

/// Poll until the stack leaves `*_IN_PROGRESS`, returning its status and reason.
async fn await_terminal_status(
    cfn: &aws_sdk_cloudformation::Client,
    stack_name: &str,
) -> (String, String) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
    loop {
        let described = cfn
            .describe_stacks()
            .stack_name(stack_name)
            .send()
            .await
            .expect("describe_stacks");
        let stack = described.stacks().first().expect("stack").clone();
        let status = stack.stack_status().unwrap().as_str().to_string();
        if !status.ends_with("_IN_PROGRESS") {
            return (
                status,
                stack.stack_status_reason().unwrap_or_default().to_string(),
            );
        }
        assert!(
            std::time::Instant::now() < deadline,
            "stack never reached a terminal status"
        );
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
}

fn build_python_handler_zip(body: &str) -> Vec<u8> {
    use std::io::Write;
    let mut buf = Vec::new();
    {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        zip.start_file("index.py", opts).unwrap();
        zip.write_all(body.as_bytes()).unwrap();
        zip.finish().unwrap();
    }
    buf
}

fn docker_available() -> bool {
    std::process::Command::new("docker")
        .arg("info")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}
