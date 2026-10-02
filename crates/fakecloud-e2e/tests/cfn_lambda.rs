//! CloudFormation provisioner for `AWS::Lambda::Function`. Covers the
//! three Code source variants (`ZipFile` inline, `S3Bucket` + `S3Key`,
//! `ImageUri`) plus an in-place stack update path that mutates Handler
//! and Environment.Variables on the live function.

mod helpers;

use aws_sdk_cloudformation::types::{Capability, OnFailure, Parameter};
use aws_sdk_lambda::primitives::Blob;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::{
    ServerSideEncryption, ServerSideEncryptionByDefault, ServerSideEncryptionConfiguration,
    ServerSideEncryptionRule,
};
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
    // Code SHA is non-empty because the deployment package bytes were hashed.
    assert!(
        !cfg.code_sha256().unwrap_or_default().is_empty(),
        "code_sha256 should be populated from ZipFile bytes"
    );

    // Every assertion above passed while the function was unrunnable: the
    // provisioner stored the inline source as though it were already an
    // archive, so the first cold start died in extract_zip with
    // "invalid Zip archive: Could not find EOCD". Only an invoke catches it.
    // The `{}` payload is load-bearing: an Invoke with no body reaches the
    // Node runtime as an empty event and dies in `JSON.parse("")`, which has
    // nothing to do with the packaging under test here.
    let invoked = lambda
        .invoke()
        .function_name(&func_name)
        .payload(Blob::new(b"{}".to_vec()))
        .send()
        .await
        .expect("invoke");
    assert!(
        invoked.function_error().is_none(),
        "inline ZipFile function failed to run: {:?}",
        invoked.function_error()
    );
    let payload = invoked
        .payload()
        .map(|p| String::from_utf8_lossy(p.as_ref()).into_owned())
        .unwrap_or_default();
    assert_eq!(payload, r#"{"ok":true}"#);

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

/// Regression: `cdk bootstrap` gives its assets bucket default `aws:kms`
/// encryption, so every asset is stored as a KMS envelope. The provisioner read
/// the stored bytes straight off S3 state, skipping the unwrap the S3 API does,
/// and handed Lambda the envelope in place of the ZIP — which surfaced at invoke
/// time as `ZIP extraction failed: invalid Zip archive: Could not find EOCD`.
/// The sibling test above passes an unencrypted bucket and so never caught it.
#[tokio::test]
async fn cfn_creates_lambda_function_from_sse_kms_s3_code() {
    let server = TestServer::start().await;
    let cfn = server.cloudformation_client().await;
    let lambda = server.lambda_client().await;
    let s3 = server.s3_client().await;
    let kms = server.kms_client().await;

    let key_id = kms
        .create_key()
        .send()
        .await
        .expect("create_key")
        .key_metadata()
        .expect("key metadata")
        .key_id()
        .to_string();

    let bucket = "cfn-lambda-kms-code-bucket";
    let key = "code/main.zip";
    s3.create_bucket()
        .bucket(bucket)
        .send()
        .await
        .expect("create_bucket");
    s3.put_bucket_encryption()
        .bucket(bucket)
        .server_side_encryption_configuration(
            ServerSideEncryptionConfiguration::builder()
                .rules(
                    ServerSideEncryptionRule::builder()
                        .apply_server_side_encryption_by_default(
                            ServerSideEncryptionByDefault::builder()
                                .sse_algorithm(ServerSideEncryption::AwsKms)
                                .kms_master_key_id(&key_id)
                                .build()
                                .unwrap(),
                        )
                        .build(),
                )
                .build()
                .unwrap(),
        )
        .send()
        .await
        .expect("put_bucket_encryption");

    let code_bytes = b"def handler(event, context): return {'ok': True}".to_vec();
    s3.put_object()
        .bucket(bucket)
        .key(key)
        .body(ByteStream::from(code_bytes.clone()))
        .send()
        .await
        .expect("put_object");

    // Guard the premise: if the bucket stopped encrypting, the assertions
    // below would pass without exercising the decrypt at all.
    let head = s3
        .head_object()
        .bucket(bucket)
        .key(key)
        .send()
        .await
        .expect("head_object");
    assert_eq!(
        head.server_side_encryption(),
        Some(&ServerSideEncryption::AwsKms)
    );

    let template = template_s3_code("cfn-lambda-kms-role");
    cfn.create_stack()
        .stack_name("cfn-lambda-kms")
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
        .stack_name("cfn-lambda-kms")
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
    // The plaintext length, not the (longer) envelope's: before the fix this
    // was the base64 KMS blob.
    assert_eq!(cfg.code_size(), code_bytes.len() as i64);
    assert_eq!(
        cfg.code_sha256(),
        Some(sha256_b64(&code_bytes).as_str()),
        "stored code must hash to the plaintext object, not the envelope"
    );
}

/// Base64 SHA-256, matching the `CodeSha256` Lambda reports.
fn sha256_b64(bytes: &[u8]) -> String {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    base64::engine::general_purpose::STANDARD.encode(hasher.finalize())
}
