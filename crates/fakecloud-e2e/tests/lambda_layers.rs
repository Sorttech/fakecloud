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
