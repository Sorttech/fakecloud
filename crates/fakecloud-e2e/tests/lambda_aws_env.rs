//! A Lambda's container gets the environment real Lambda provides, pointed
//! at fakecloud instead of real AWS.
//!
//! Real Lambda injects region, execution-role credentials and function
//! metadata, and function code relies on them. fakecloud injected none, and
//! nothing pointed the SDK at the emulator, so handler code that called AWS
//! silently targeted the internet: CDK's `BucketDeployment` reported success
//! having copied no files.

mod helpers;

use aws_sdk_lambda::primitives::Blob;
use aws_sdk_lambda::types::{Environment, FunctionCode, Runtime};
use helpers::TestServer;

fn docker_available() -> bool {
    std::process::Command::new("docker")
        .arg("info")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn build_python_handler_zip(body: &str) -> Vec<u8> {
    use std::io::Write;
    let mut buf = Vec::new();
    {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let opts: zip::write::FileOptions<'_, ()> =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Stored);
        zip.start_file("index.py", opts).unwrap();
        zip.write_all(body.as_bytes()).unwrap();
        zip.finish().unwrap();
    }
    buf
}

/// SDK config signed with the reserved root-bypass credentials, which skip
/// SigV4 verification and IAM enforcement.
async fn root_config(server: &TestServer) -> aws_config::SdkConfig {
    aws_config::defaults(aws_config::BehaviorVersion::latest())
        .endpoint_url(server.endpoint())
        .region(aws_config::Region::new("us-east-1"))
        .credentials_provider(aws_sdk_lambda::config::Credentials::new(
            "test", "test", None, None, "test",
        ))
        .load()
        .await
}

/// The handler reports the environment it sees and the identity its SDK
/// calls resolve to. Signature verification is on, so the call only
/// succeeds if the injected credentials are registered with fakecloud.
const PROBE_HANDLER: &str = r#"import os
import boto3

KEYS = (
    "AWS_ENDPOINT_URL", "AWS_REGION", "AWS_DEFAULT_REGION",
    "AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY", "AWS_SESSION_TOKEN",
    "AWS_LAMBDA_FUNCTION_NAME", "AWS_LAMBDA_FUNCTION_VERSION",
    "AWS_LAMBDA_FUNCTION_MEMORY_SIZE", "AWS_LAMBDA_LOG_GROUP_NAME",
    "AWS_EXECUTION_ENV", "APP_SETTING",
)

def handler(event, context):
    out = {k: os.environ.get(k) for k in KEYS}
    out["caller_arn"] = boto3.client("sts").get_caller_identity()["Arn"]
    out["context_function_name"] = context.function_name
    return out
"#;

#[tokio::test]
async fn lambda_container_receives_the_aws_environment() {
    if !docker_available() {
        eprintln!("docker required for Lambda execution; skipping");
        return;
    }
    let server = TestServer::start_with_env(&[("FAKECLOUD_VERIFY_SIGV4", "true")]).await;
    // The test itself drives the control plane with the root-bypass
    // credentials; only the function's own calls are verified.
    let root = root_config(&server).await;
    let lambda = aws_sdk_lambda::Client::new(&root);

    lambda
        .create_function()
        .function_name("env-probe-fn")
        .runtime(Runtime::Python312)
        .role("arn:aws:iam::123456789012:role/env-probe-role")
        .handler("index.handler")
        .memory_size(256)
        .timeout(30)
        .environment(
            Environment::builder()
                .variables("APP_SETTING", "kept")
                .build(),
        )
        .code(
            FunctionCode::builder()
                .zip_file(build_python_handler_zip(PROBE_HANDLER).into())
                .build(),
        )
        .send()
        .await
        .expect("create_function");

    let invoked = lambda
        .invoke()
        .function_name("env-probe-fn")
        .payload(Blob::new("{}"))
        .send()
        .await
        .expect("invoke");
    let payload = String::from_utf8(
        invoked
            .payload()
            .map(|b| b.as_ref().to_vec())
            .unwrap_or_default(),
    )
    .unwrap_or_default();
    assert!(
        invoked.function_error().is_none(),
        "handler errored: {payload}"
    );
    let env: serde_json::Value = serde_json::from_str(&payload).expect("handler returned JSON");
    let get = |k: &str| env[k].as_str().unwrap_or_default().to_string();

    // Points back at fakecloud on the host, on the port this server bound,
    // never `localhost` (inside the container that is the container itself).
    let endpoint = get("AWS_ENDPOINT_URL");
    assert!(
        endpoint.ends_with(&format!(":{}", server.port())),
        "endpoint {endpoint} should target this server's port {}",
        server.port()
    );
    assert!(
        !endpoint.contains("localhost:") && !endpoint.contains("127.0.0.1"),
        "endpoint {endpoint} must use the container's host alias"
    );

    // The SDK inside the function signs as the execution role, in a session
    // named after the function, as on AWS. Under --verify-sigv4 this only
    // works because the credentials are registered, not placeholders.
    assert_eq!(
        get("caller_arn"),
        "arn:aws:sts::123456789012:assumed-role/env-probe-role/env-probe-fn",
        "{payload}"
    );
    assert!(!get("AWS_SESSION_TOKEN").is_empty(), "{payload}");

    assert_eq!(get("AWS_REGION"), "us-east-1", "{payload}");
    assert_eq!(get("AWS_DEFAULT_REGION"), "us-east-1", "{payload}");
    assert_eq!(get("AWS_LAMBDA_FUNCTION_NAME"), "env-probe-fn");
    assert_eq!(get("context_function_name"), "env-probe-fn");
    assert_eq!(get("AWS_LAMBDA_FUNCTION_VERSION"), "$LATEST");
    assert_eq!(get("AWS_LAMBDA_FUNCTION_MEMORY_SIZE"), "256");
    assert_eq!(get("AWS_LAMBDA_LOG_GROUP_NAME"), "/aws/lambda/env-probe-fn");
    assert_eq!(get("AWS_EXECUTION_ENV"), "AWS_Lambda_python3.12");
    assert_eq!(get("APP_SETTING"), "kept");

    // A configuration change reaches the next invocation instead of being
    // masked by the warm container.
    lambda
        .update_function_configuration()
        .function_name("env-probe-fn")
        .environment(
            Environment::builder()
                .variables("APP_SETTING", "changed")
                .build(),
        )
        .send()
        .await
        .expect("update_function_configuration");
    let invoked = lambda
        .invoke()
        .function_name("env-probe-fn")
        .payload(Blob::new("{}"))
        .send()
        .await
        .expect("invoke after update");
    let payload = String::from_utf8(invoked.payload().unwrap().as_ref().to_vec()).unwrap();
    let env: serde_json::Value = serde_json::from_str(&payload).expect("handler returned JSON");
    assert_eq!(env["APP_SETTING"], "changed", "{payload}");
}

/// Lambda reserves the keys it sets itself; a configuration that sets one is
/// rejected on create and on update, as on AWS.
#[tokio::test]
async fn reserved_environment_keys_are_rejected() {
    let server = TestServer::start().await;
    let lambda = server.lambda_client().await;
    let code = || {
        FunctionCode::builder()
            .zip_file(build_python_handler_zip("def handler(e, c):\n    return 1\n").into())
            .build()
    };

    let err = lambda
        .create_function()
        .function_name("reserved-env-fn")
        .runtime(Runtime::Python312)
        .role("arn:aws:iam::123456789012:role/r")
        .handler("index.handler")
        .environment(
            Environment::builder()
                .variables("AWS_REGION", "eu-west-1")
                .variables("AWS_SESSION_TOKEN", "x")
                .build(),
        )
        .code(code())
        .send()
        .await
        .expect_err("reserved keys must be rejected");
    let service_err = err.into_service_error();
    assert!(
        service_err.is_invalid_parameter_value_exception(),
        "{service_err:?}"
    );
    let message = service_err.meta().message().unwrap_or_default().to_string();
    assert!(
        message.ends_with("Reserved keys used in this request: AWS_REGION, AWS_SESSION_TOKEN"),
        "{message}"
    );

    // Not reserved: a function may point its SDK somewhere else.
    lambda
        .create_function()
        .function_name("reserved-env-fn")
        .runtime(Runtime::Python312)
        .role("arn:aws:iam::123456789012:role/r")
        .handler("index.handler")
        .environment(
            Environment::builder()
                .variables("AWS_ENDPOINT_URL", "http://localhost:4566")
                .build(),
        )
        .code(code())
        .send()
        .await
        .expect("AWS_ENDPOINT_URL is not reserved");

    let err = lambda
        .update_function_configuration()
        .function_name("reserved-env-fn")
        .environment(
            Environment::builder()
                .variables("AWS_ACCESS_KEY_ID", "AKIA")
                .build(),
        )
        .send()
        .await
        .expect_err("reserved keys must be rejected on update");
    assert!(err
        .into_service_error()
        .is_invalid_parameter_value_exception());
    let cfg = lambda
        .get_function_configuration()
        .function_name("reserved-env-fn")
        .send()
        .await
        .unwrap();
    let vars = cfg.environment().and_then(|e| e.variables()).unwrap();
    assert!(
        vars.contains_key("AWS_ENDPOINT_URL") && !vars.contains_key("AWS_ACCESS_KEY_ID"),
        "a rejected update must not apply: {vars:?}"
    );
}

/// Under IAM enforcement `iam:PassRole` is same-account only, and the role
/// must trust Lambda: both CreateFunction and UpdateFunctionConfiguration
/// enforce it.
#[tokio::test]
async fn execution_role_must_be_same_account_and_trust_lambda() {
    let server = TestServer::start_with_env(&[("FAKECLOUD_IAM", "strict")]).await;
    let root = root_config(&server).await;
    let lambda = aws_sdk_lambda::Client::new(&root);
    let iam = aws_sdk_iam::Client::new(&root);
    let code = || {
        FunctionCode::builder()
            .zip_file(build_python_handler_zip("def handler(e, c):\n    return 1\n").into())
            .build()
    };

    let err = lambda
        .create_function()
        .function_name("cross-account-fn")
        .runtime(Runtime::Python312)
        .role("arn:aws:iam::999999999999:role/elsewhere")
        .handler("index.handler")
        .code(code())
        .send()
        .await
        .expect_err("a role in another account must be refused");
    let raw = err.raw_response().map(|r| r.status().as_u16());
    let service_err = err.into_service_error();
    assert_eq!(service_err.meta().code(), Some("AccessDeniedException"));
    assert_eq!(
        service_err.meta().message(),
        Some("Cross-account pass role is not allowed.")
    );
    assert_eq!(raw, Some(403));

    lambda
        .create_function()
        .function_name("role-checked-fn")
        .runtime(Runtime::Python312)
        .role("arn:aws:iam::123456789012:role/r")
        .handler("index.handler")
        .code(code())
        .send()
        .await
        .expect("same-account role");

    // A role whose trust policy does not name Lambda cannot be swapped in.
    let untrusted = iam
        .create_role()
        .role_name("not-for-lambda")
        .assume_role_policy_document(
            r#"{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Principal":{"Service":"ec2.amazonaws.com"},"Action":"sts:AssumeRole"}]}"#,
        )
        .send()
        .await
        .unwrap();
    let err = lambda
        .update_function_configuration()
        .function_name("role-checked-fn")
        .role(untrusted.role().unwrap().arn())
        .send()
        .await
        .expect_err("an untrusted role must be refused on update");
    assert!(err
        .into_service_error()
        .is_invalid_parameter_value_exception());

    let err = lambda
        .update_function_configuration()
        .function_name("role-checked-fn")
        .role("arn:aws:iam::999999999999:role/elsewhere")
        .send()
        .await
        .expect_err("a cross-account role must be refused on update");
    assert_eq!(
        err.into_service_error().meta().code(),
        Some("AccessDeniedException")
    );
    let cfg = lambda
        .get_function_configuration()
        .function_name("role-checked-fn")
        .send()
        .await
        .unwrap();
    assert_eq!(cfg.role(), Some("arn:aws:iam::123456789012:role/r"));
}

/// With IAM enforcement off (the default) a role ARN naming another account,
/// as templates written for other emulators do (`000000000000`), is still
/// accepted on create and update; the trust check applies as before.
#[tokio::test]
async fn default_mode_accepts_another_accounts_role() {
    let server = TestServer::start().await;
    let lambda = server.lambda_client().await;
    lambda
        .create_function()
        .function_name("foreign-role-fn")
        .runtime(Runtime::Python312)
        .role("arn:aws:iam::000000000000:role/lambda-role")
        .handler("index.handler")
        .code(
            FunctionCode::builder()
                .zip_file(build_python_handler_zip("def handler(e, c):\n    return 1\n").into())
                .build(),
        )
        .send()
        .await
        .expect("default mode accepts a role from another account");
    lambda
        .update_function_configuration()
        .function_name("foreign-role-fn")
        .role("arn:aws:iam::111111111111:role/other")
        .send()
        .await
        .expect("and on update");
}
