//! Lambda layer attachment + runtime mount.
//!
//! Reproduces the bug from issue #853: `Layers` field on
//! Create/UpdateFunctionConfiguration was silently dropped, so layer
//! modules were not on `sys.path` when the function executed.
//!
//! Wire round-trip is asserted unconditionally. The actual `import`
//! check is gated on Docker since it boots a Python Lambda container.

mod helpers;

use std::io::Write;

use aws_sdk_lambda::primitives::Blob;
use aws_sdk_lambda::types::{FunctionCode, LayerVersionContentInput, Runtime};
use helpers::TestServer;
use sha2::{Digest, Sha256};

fn docker_available() -> bool {
    std::process::Command::new("docker")
        .arg("info")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn make_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let buf = Vec::new();
    let cursor = std::io::Cursor::new(buf);
    let mut writer = zip::ZipWriter::new(cursor);
    let options = zip::write::SimpleFileOptions::default().unix_permissions(0o755);
    for (name, content) in entries {
        writer.start_file(*name, options).unwrap();
        writer.write_all(content).unwrap();
    }
    writer.finish().unwrap().into_inner()
}

fn b64_sha256(bytes: &[u8]) -> String {
    use base64::Engine;
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    base64::engine::general_purpose::STANDARD.encode(hasher.finalize())
}

#[tokio::test]
async fn publish_layer_returns_real_content_metadata_and_downloadable_location() {
    let server = TestServer::start().await;
    let lambda = server.lambda_client().await;

    let zip = make_zip(&[("python/greeter.py", b"def greet():\n    return 'hi'\n")]);
    let expected_sha = b64_sha256(&zip);
    let expected_size = zip.len() as i64;

    let pub_resp = lambda
        .publish_layer_version()
        .layer_name("greeter")
        .content(
            LayerVersionContentInput::builder()
                .zip_file(Blob::new(zip.clone()))
                .build(),
        )
        .send()
        .await
        .unwrap();

    let content = pub_resp.content().expect("Content set on publish response");
    assert_eq!(content.code_sha256(), Some(expected_sha.as_str()));
    assert_eq!(content.code_size(), expected_size);
    let location = content
        .location()
        .expect("Location set on publish response");
    assert!(location.contains("/_fakecloud/lambda/layer-content/"));

    // Location must be a real URL that streams the original ZIP.
    let bytes = reqwest::get(location)
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert_eq!(bytes.as_ref(), zip.as_slice());

    // GetLayerVersion mirrors the publish payload.
    let get_resp = lambda
        .get_layer_version()
        .layer_name("greeter")
        .version_number(1)
        .send()
        .await
        .unwrap();
    let g_content = get_resp
        .content()
        .expect("Content set on GetLayerVersion response");
    assert_eq!(g_content.code_sha256(), Some(expected_sha.as_str()));
    assert_eq!(g_content.code_size(), expected_size);
}

#[tokio::test]
async fn create_and_update_function_round_trips_layers() {
    let server = TestServer::start().await;
    let lambda = server.lambda_client().await;

    let layer_a = make_zip(&[("python/a.py", b"def a():\n    return 1\n")]);
    let layer_b = make_zip(&[("python/b.py", b"def b():\n    return 2\n")]);

    let arn_a = lambda
        .publish_layer_version()
        .layer_name("layer-a")
        .content(
            LayerVersionContentInput::builder()
                .zip_file(Blob::new(layer_a.clone()))
                .build(),
        )
        .send()
        .await
        .unwrap()
        .layer_version_arn()
        .unwrap()
        .to_string();
    let arn_b = lambda
        .publish_layer_version()
        .layer_name("layer-b")
        .content(
            LayerVersionContentInput::builder()
                .zip_file(Blob::new(layer_b.clone()))
                .build(),
        )
        .send()
        .await
        .unwrap()
        .layer_version_arn()
        .unwrap()
        .to_string();

    let handler_zip = make_zip(&[("index.py", b"def handler(e, c):\n    return {}\n")]);
    let create = lambda
        .create_function()
        .function_name("layered-fn")
        .runtime(Runtime::Python312)
        .role("arn:aws:iam::123456789012:role/r")
        .handler("index.handler")
        .code(
            FunctionCode::builder()
                .zip_file(Blob::new(handler_zip))
                .build(),
        )
        .layers(arn_a.clone())
        .layers(arn_b.clone())
        .send()
        .await
        .unwrap();
    let layers = create.layers();
    assert_eq!(layers.len(), 2);
    assert_eq!(layers[0].arn(), Some(arn_a.as_str()));
    assert_eq!(layers[0].code_size(), layer_a.len() as i64);
    assert_eq!(layers[1].arn(), Some(arn_b.as_str()));
    assert_eq!(layers[1].code_size(), layer_b.len() as i64);

    let cfg = lambda
        .get_function_configuration()
        .function_name("layered-fn")
        .send()
        .await
        .unwrap();
    let g_layers = cfg.layers();
    assert_eq!(g_layers.len(), 2);
    assert_eq!(g_layers[0].arn(), Some(arn_a.as_str()));
    assert_eq!(g_layers[1].arn(), Some(arn_b.as_str()));

    // Replace with reversed order — assert order preserved
    let updated = lambda
        .update_function_configuration()
        .function_name("layered-fn")
        .layers(arn_b.clone())
        .layers(arn_a.clone())
        .send()
        .await
        .unwrap();
    let u_layers = updated.layers();
    assert_eq!(u_layers.len(), 2);
    assert_eq!(u_layers[0].arn(), Some(arn_b.as_str()));
    assert_eq!(u_layers[1].arn(), Some(arn_a.as_str()));

    // Empty list detaches all layers
    let detached = lambda
        .update_function_configuration()
        .function_name("layered-fn")
        .set_layers(Some(vec![]))
        .send()
        .await
        .unwrap();
    assert!(
        detached.layers().is_empty(),
        "Layers should be empty after explicit reset"
    );
}

#[tokio::test]
async fn invoke_python_function_imports_module_from_attached_layer() {
    if !docker_available() {
        eprintln!("docker required for Lambda execution; skipping");
        return;
    }
    let server = TestServer::start().await;
    let lambda = server.lambda_client().await;

    // Layer ZIP places `greeter.py` under `python/`, the Python-runtime
    // path AWS base images add to `sys.path` for `/opt`.
    let layer_zip = make_zip(&[(
        "python/greeter.py",
        b"def greet():\n    return 'hello-from-layer'\n",
    )]);
    let layer_arn = lambda
        .publish_layer_version()
        .layer_name("invoke-greeter")
        .content(
            LayerVersionContentInput::builder()
                .zip_file(Blob::new(layer_zip))
                .build(),
        )
        .compatible_runtimes(Runtime::Python312)
        .send()
        .await
        .unwrap()
        .layer_version_arn()
        .unwrap()
        .to_string();

    let handler_src = b"\
import greeter
def handler(event, context):
    return {'msg': greeter.greet()}
";
    let handler_zip = make_zip(&[("index.py", handler_src)]);
    lambda
        .create_function()
        .function_name("layered-invoke")
        .runtime(Runtime::Python312)
        .role("arn:aws:iam::123456789012:role/r")
        .handler("index.handler")
        .code(
            FunctionCode::builder()
                .zip_file(Blob::new(handler_zip))
                .build(),
        )
        .layers(layer_arn)
        .send()
        .await
        .unwrap();

    let resp = lambda
        .invoke()
        .function_name("layered-invoke")
        .payload(Blob::new(b"{}".to_vec()))
        .send()
        .await
        .unwrap();
    let payload = String::from_utf8(resp.payload().unwrap().as_ref().to_vec()).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&payload).unwrap();
    assert_eq!(
        parsed.get("msg").and_then(|v| v.as_str()),
        Some("hello-from-layer"),
        "function response: {payload}"
    );
}

// ── Cross-account layer sharing ──
//
// Account A owns the layer; B and C name it by ARN. A layer version is
// readable and attachable from another account only once its policy grants
// `lambda:GetLayerVersion` to that account (or to its organization).

const LAYER_OWNER: &str = "111111111111";
const LAYER_USER: &str = "222222222222";
const OUTSIDER: &str = "333333333333";
const XACCT_REGION: &str = "us-east-1";

async fn start_multi_account() -> TestServer {
    TestServer::start_with_env(&[
        ("FAKECLOUD_IAM", "strict"),
        ("FAKECLOUD_VERIFY_SIGV4", "true"),
        ("FAKECLOUD_CONTAINER_CLI", "false"),
    ])
    .await
}

async fn sdk_config(server: &TestServer, akid: &str, secret: &str) -> aws_config::SdkConfig {
    aws_config::defaults(aws_config::BehaviorVersion::latest())
        .endpoint_url(server.endpoint())
        .region(aws_config::Region::new(XACCT_REGION))
        .credentials_provider(aws_credential_types::Credentials::new(
            akid,
            secret,
            None,
            None,
            "layers-x-acct",
        ))
        .load()
        .await
}

async fn admin_config(server: &TestServer, account: &str) -> aws_config::SdkConfig {
    let (akid, secret) = server.create_admin(account, "admin").await;
    sdk_config(server, &akid, &secret).await
}

fn owner_layer_arn(name: &str) -> String {
    format!("arn:aws:lambda:{XACCT_REGION}:{LAYER_OWNER}:layer:{name}")
}

async fn publish_zip(lambda: &aws_sdk_lambda::Client, name: &str) -> i64 {
    lambda
        .publish_layer_version()
        .layer_name(name)
        .content(
            LayerVersionContentInput::builder()
                .zip_file(Blob::new(make_zip(&[("python/shared.py", b"X = 1\n")])))
                .build(),
        )
        .send()
        .await
        .expect("PublishLayerVersion")
        .version()
}

/// The error code and message of a failed SDK call.
fn error_of<T, E>(result: Result<T, aws_sdk_lambda::error::SdkError<E>>) -> (String, String)
where
    aws_sdk_lambda::error::SdkError<E>: aws_sdk_lambda::error::ProvideErrorMetadata,
{
    use aws_sdk_lambda::error::ProvideErrorMetadata;
    let err = result.err().expect("call must fail");
    (
        err.code().unwrap_or_default().to_string(),
        err.message().unwrap_or_default().to_string(),
    )
}

/// An execution role Lambda may assume, in the caller's account.
async fn lambda_role(cfg: &aws_config::SdkConfig, account: &str) -> String {
    aws_sdk_iam::Client::new(cfg)
        .create_role()
        .role_name("layer-user-role")
        .assume_role_policy_document(
            r#"{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Principal":{"Service":"lambda.amazonaws.com"},"Action":"sts:AssumeRole"}]}"#,
        )
        .send()
        .await
        .expect("CreateRole");
    format!("arn:aws:iam::{account}:role/layer-user-role")
}

async fn create_with_layers(
    lambda: &aws_sdk_lambda::Client,
    role: &str,
    name: &str,
    layers: &[String],
) -> Result<
    aws_sdk_lambda::operation::create_function::CreateFunctionOutput,
    aws_sdk_lambda::error::SdkError<
        aws_sdk_lambda::operation::create_function::CreateFunctionError,
    >,
> {
    let mut req = lambda
        .create_function()
        .function_name(name)
        .runtime(Runtime::Python312)
        .role(role)
        .handler("index.handler")
        .code(
            FunctionCode::builder()
                .zip_file(Blob::new(make_zip(&[(
                    "index.py",
                    b"def handler(e, c):\n    return 1\n",
                )])))
                .build(),
        );
    for layer in layers {
        req = req.layers(layer);
    }
    req.send().await
}

#[tokio::test]
async fn a_shared_layer_version_is_readable_and_attachable_cross_account() {
    let server = start_multi_account().await;
    let owner = aws_sdk_lambda::Client::new(&admin_config(&server, LAYER_OWNER).await);
    let user_cfg = admin_config(&server, LAYER_USER).await;
    let user = aws_sdk_lambda::Client::new(&user_cfg);
    assert_eq!(publish_zip(&owner, "shared").await, 1);
    assert_eq!(publish_zip(&owner, "shared").await, 2);
    let arn = owner_layer_arn("shared");
    let v1 = format!("{arn}:1");
    let v2 = format!("{arn}:2");

    // Before any grant: refused, and a version that does not exist is
    // refused in exactly the same way.
    let unshared = error_of(
        user.get_layer_version()
            .layer_name(&arn)
            .version_number(1)
            .send()
            .await,
    );
    let missing = error_of(
        user.get_layer_version()
            .layer_name(&arn)
            .version_number(42)
            .send()
            .await,
    );
    assert_eq!(unshared.0, "AccessDeniedException", "{}", unshared.1);
    assert_eq!(missing.0, "AccessDeniedException", "{}", missing.1);
    assert_eq!(
        unshared.1.replace(":shared:1 ", ":shared:N "),
        missing.1.replace(":shared:42 ", ":shared:N ")
    );
    assert!(
        unshared.1.contains("lambda:GetLayerVersion"),
        "{}",
        unshared.1
    );

    let role = lambda_role(&user_cfg, LAYER_USER).await;
    let refused =
        error_of(create_with_layers(&user, &role, "consumer", std::slice::from_ref(&v1)).await);
    assert_eq!(refused.0, "AccessDeniedException", "{}", refused.1);

    let granted = owner
        .add_layer_version_permission()
        .layer_name("shared")
        .version_number(1)
        .statement_id("to-user")
        .action("lambda:GetLayerVersion")
        .principal(LAYER_USER)
        .send()
        .await
        .expect("AddLayerVersionPermission");
    let stmt: serde_json::Value = serde_json::from_str(granted.statement().unwrap()).unwrap();
    assert_eq!(
        stmt["Principal"]["AWS"],
        format!("arn:aws:iam::{LAYER_USER}:root")
    );

    let got = user
        .get_layer_version()
        .layer_name(&arn)
        .version_number(1)
        .send()
        .await
        .expect("shared version is readable");
    assert_eq!(got.layer_version_arn(), Some(v1.as_str()));
    let by_arn = user
        .get_layer_version_by_arn()
        .arn(&v1)
        .send()
        .await
        .expect("GetLayerVersionByArn on a shared version");
    assert_eq!(by_arn.version(), 1);
    assert_eq!(by_arn.layer_arn(), Some(arn.as_str()));
    // Version 2 was not shared.
    let (code, _) = error_of(user.get_layer_version_by_arn().arn(&v2).send().await);
    assert_eq!(code, "AccessDeniedException");
    // A version policy never grants listing another account's versions.
    let (code, msg) = error_of(user.list_layer_versions().layer_name(&arn).send().await);
    assert_eq!(code, "AccessDeniedException");
    assert!(msg.contains("lambda:ListLayerVersions"), "{msg}");

    let created = create_with_layers(&user, &role, "consumer", std::slice::from_ref(&v1))
        .await
        .expect("a shared layer attaches");
    let attached = created.layers();
    assert_eq!(attached.len(), 1);
    assert_eq!(attached[0].arn(), Some(v1.as_str()));
    assert!(attached[0].code_size() > 0);
    let (code, _) = error_of(
        user.update_function_configuration()
            .function_name("consumer")
            .layers(&v2)
            .send()
            .await,
    );
    assert_eq!(code, "AccessDeniedException");

    // Writing through the owner's ARN is refused and creates nothing on
    // either side.
    let (code, msg) = error_of(
        user.publish_layer_version()
            .layer_name(&arn)
            .content(
                LayerVersionContentInput::builder()
                    .zip_file(Blob::new(make_zip(&[("x", b"x")])))
                    .build(),
            )
            .send()
            .await,
    );
    assert_eq!(code, "AccessDeniedException");
    assert!(msg.contains("lambda:PublishLayerVersion"), "{msg}");
    let (code, _) = error_of(
        user.delete_layer_version()
            .layer_name(&arn)
            .version_number(1)
            .send()
            .await,
    );
    assert_eq!(code, "AccessDeniedException");
    let (code, _) = error_of(
        user.add_layer_version_permission()
            .layer_name(&arn)
            .version_number(2)
            .statement_id("self-grant")
            .action("lambda:GetLayerVersion")
            .principal(LAYER_USER)
            .send()
            .await,
    );
    assert_eq!(code, "AccessDeniedException");
    let owner_versions = owner
        .list_layer_versions()
        .layer_name("shared")
        .send()
        .await
        .unwrap();
    assert_eq!(owner_versions.layer_versions().len(), 2);
    assert!(user.list_layers().send().await.unwrap().layers().is_empty());
}

#[tokio::test]
async fn an_organization_scoped_grant_admits_member_accounts_only() {
    let server = start_multi_account().await;
    let owner_cfg = admin_config(&server, LAYER_OWNER).await;
    let org_id = aws_sdk_organizations::Client::new(&owner_cfg)
        .create_organization()
        .send()
        .await
        .unwrap()
        .organization()
        .unwrap()
        .id()
        .unwrap()
        .to_string();
    let (akid, secret) = server
        .create_admin_in_org(LAYER_USER, "member", &org_id)
        .await;
    let member = aws_sdk_lambda::Client::new(&sdk_config(&server, &akid, &secret).await);
    let outsider = aws_sdk_lambda::Client::new(&admin_config(&server, OUTSIDER).await);
    let owner = aws_sdk_lambda::Client::new(&owner_cfg);

    publish_zip(&owner, "org-layer").await;
    let granted = owner
        .add_layer_version_permission()
        .layer_name("org-layer")
        .version_number(1)
        .statement_id("org")
        .action("lambda:GetLayerVersion")
        .principal("*")
        .organization_id(&org_id)
        .send()
        .await
        .expect("org-scoped grant");
    let stmt: serde_json::Value = serde_json::from_str(granted.statement().unwrap()).unwrap();
    assert_eq!(
        stmt["Condition"]["StringEquals"]["aws:PrincipalOrgID"],
        org_id
    );

    let v1 = format!("{}:1", owner_layer_arn("org-layer"));
    member
        .get_layer_version_by_arn()
        .arn(&v1)
        .send()
        .await
        .expect("a member of the organization reads the layer");
    let (code, _) = error_of(outsider.get_layer_version_by_arn().arn(&v1).send().await);
    assert_eq!(code, "AccessDeniedException");
}

/// `LayerName` outside its `@pattern` is rejected before any lookup: the
/// publish must not create a layer, and the otherwise idempotent delete and
/// the listing fail the same way.
#[tokio::test]
async fn malformed_layer_name_is_rejected_by_every_layer_op() {
    let server = TestServer::start().await;
    let lambda = server.lambda_client().await;
    let bad = "bad!name";

    let err = lambda
        .publish_layer_version()
        .layer_name(bad)
        .content(
            LayerVersionContentInput::builder()
                .zip_file(Blob::new(make_zip(&[("a.txt", b"a")])))
                .build(),
        )
        .send()
        .await
        .expect_err("publish with a malformed LayerName");
    assert_eq!(
        err.into_service_error().meta().code(),
        Some("ValidationException")
    );

    let err = lambda
        .list_layer_versions()
        .layer_name(bad)
        .send()
        .await
        .expect_err("list with a malformed LayerName");
    assert_eq!(
        err.into_service_error().meta().code(),
        Some("ValidationException")
    );

    let err = lambda
        .delete_layer_version()
        .layer_name(bad)
        .version_number(1)
        .send()
        .await
        .expect_err("delete with a malformed LayerName");
    assert_eq!(
        err.into_service_error().meta().code(),
        Some("ValidationException")
    );

    let layers = lambda.list_layers().send().await.unwrap();
    assert!(layers.layers().is_empty(), "no layer may have been created");

    // A well-formed name for a layer that never existed is still a no-op
    // delete, as in AWS.
    lambda
        .delete_layer_version()
        .layer_name("never-published")
        .version_number(1)
        .send()
        .await
        .expect("idempotent delete of an unknown layer version");
}
