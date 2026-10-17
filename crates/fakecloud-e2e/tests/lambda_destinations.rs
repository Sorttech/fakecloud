//! Lambda async invocations route their result to the configured
//! OnSuccess/OnFailure destination (SQS / SNS / EventBridge / Lambda).
//! Test gates on Docker since real Lambda execution is required.

mod helpers;

use std::time::Duration;

use aws_sdk_lambda::primitives::Blob;
use aws_sdk_lambda::types::{
    DestinationConfig as LambdaDestinationConfig, FunctionCode, InvocationType,
    OnFailure as LambdaOnFailure, OnSuccess as LambdaOnSuccess, Runtime,
};
use aws_sdk_sqs::types::QueueAttributeName;
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
        let opts: zip::write::SimpleFileOptions =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Stored);
        zip.start_file("index.py", opts).unwrap();
        zip.write_all(body.as_bytes()).unwrap();
        zip.finish().unwrap();
    }
    buf
}

#[tokio::test]
async fn lambda_async_invoke_routes_on_success_to_sqs() {
    if !docker_available() {
        eprintln!("docker required for Lambda execution; skipping");
        return;
    }
    let server = TestServer::start().await;
    let lambda = server.lambda_client().await;
    let iam = server.iam_client().await;
    let sqs = server.sqs_client().await;

    let q = sqs
        .create_queue()
        .queue_name("destinations-success")
        .send()
        .await
        .unwrap();
    let queue_url = q.queue_url().unwrap().to_string();
    let queue_arn = sqs
        .get_queue_attributes()
        .queue_url(&queue_url)
        .attribute_names(QueueAttributeName::QueueArn)
        .send()
        .await
        .unwrap()
        .attributes()
        .unwrap()
        .get(&QueueAttributeName::QueueArn)
        .unwrap()
        .to_string();

    iam.create_role()
        .role_name("dest-success-role")
        .assume_role_policy_document(
            r#"{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Principal":{"Service":"lambda.amazonaws.com"},"Action":"sts:AssumeRole"}]}"#,
        )
        .send()
        .await
        .unwrap();
    let zip_bytes = build_python_handler_zip(
        "def handler(event, context):\n    return {'ok': True, 'echo': event}\n",
    );
    lambda
        .create_function()
        .function_name("dest-success-fn")
        .runtime(Runtime::Python311)
        .role("arn:aws:iam::123456789012:role/dest-success-role")
        .handler("index.handler")
        .code(FunctionCode::builder().zip_file(zip_bytes.into()).build())
        .send()
        .await
        .unwrap();

    lambda
        .put_function_event_invoke_config()
        .function_name("dest-success-fn")
        .destination_config(
            LambdaDestinationConfig::builder()
                .on_success(LambdaOnSuccess::builder().destination(&queue_arn).build())
                .build(),
        )
        .send()
        .await
        .unwrap();

    lambda
        .invoke()
        .function_name("dest-success-fn")
        .invocation_type(InvocationType::Event)
        .payload(Blob::new(br#"{"hello":"world"}"#.as_slice()))
        .send()
        .await
        .unwrap();

    // SQS receive — destination record should arrive shortly.
    let mut saw = false;
    for _ in 0..30 {
        let recv = sqs
            .receive_message()
            .queue_url(&queue_url)
            .max_number_of_messages(10)
            .wait_time_seconds(1)
            .send()
            .await
            .unwrap();
        for m in recv.messages() {
            let body = m.body().unwrap_or("");
            let v: serde_json::Value = match serde_json::from_str(body) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if v["requestContext"]["condition"].as_str() == Some("Success")
                && v["responsePayload"]["ok"].as_bool() == Some(true)
            {
                saw = true;
            }
        }
        if saw {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(
        saw,
        "expected OnSuccess destination record on SQS for async invoke"
    );
}

#[tokio::test]
async fn lambda_async_invoke_routes_on_failure_to_sns() {
    if !docker_available() {
        eprintln!("docker required for Lambda execution; skipping");
        return;
    }
    let server = TestServer::start().await;
    let lambda = server.lambda_client().await;
    let iam = server.iam_client().await;
    let sqs = server.sqs_client().await;
    let sns = server.sns_client().await;

    // SNS topic + SQS subscription so we can read what landed.
    let topic = sns
        .create_topic()
        .name("dest-failure-topic")
        .send()
        .await
        .unwrap();
    let topic_arn = topic.topic_arn().unwrap().to_string();
    let q = sqs
        .create_queue()
        .queue_name("dest-failure-q")
        .send()
        .await
        .unwrap();
    let queue_url = q.queue_url().unwrap().to_string();
    let queue_arn = sqs
        .get_queue_attributes()
        .queue_url(&queue_url)
        .attribute_names(QueueAttributeName::QueueArn)
        .send()
        .await
        .unwrap()
        .attributes()
        .unwrap()
        .get(&QueueAttributeName::QueueArn)
        .unwrap()
        .to_string();
    sns.subscribe()
        .topic_arn(&topic_arn)
        .protocol("sqs")
        .endpoint(&queue_arn)
        .send()
        .await
        .unwrap();

    iam.create_role()
        .role_name("dest-failure-role")
        .assume_role_policy_document(
            r#"{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Principal":{"Service":"lambda.amazonaws.com"},"Action":"sts:AssumeRole"}]}"#,
        )
        .send()
        .await
        .unwrap();
    let zip_bytes =
        build_python_handler_zip("def handler(event, context):\n    raise Exception('boom')\n");
    lambda
        .create_function()
        .function_name("dest-failure-fn")
        .runtime(Runtime::Python311)
        .role("arn:aws:iam::123456789012:role/dest-failure-role")
        .handler("index.handler")
        .code(FunctionCode::builder().zip_file(zip_bytes.into()).build())
        .send()
        .await
        .unwrap();
    lambda
        .put_function_event_invoke_config()
        .function_name("dest-failure-fn")
        .destination_config(
            LambdaDestinationConfig::builder()
                .on_failure(LambdaOnFailure::builder().destination(&topic_arn).build())
                .build(),
        )
        .send()
        .await
        .unwrap();
    lambda
        .invoke()
        .function_name("dest-failure-fn")
        .invocation_type(InvocationType::Event)
        .payload(Blob::new(br#"{}"#.as_slice()))
        .send()
        .await
        .unwrap();

    let mut saw = false;
    for _ in 0..30 {
        let recv = sqs
            .receive_message()
            .queue_url(&queue_url)
            .max_number_of_messages(10)
            .wait_time_seconds(1)
            .send()
            .await
            .unwrap();
        for m in recv.messages() {
            let body = m.body().unwrap_or("");
            let env: serde_json::Value = match serde_json::from_str(body) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let inner_str = env.get("Message").and_then(|v| v.as_str()).unwrap_or("");
            let inner: serde_json::Value =
                serde_json::from_str(inner_str).unwrap_or(serde_json::Value::Null);
            if inner["requestContext"]["condition"].as_str() == Some("RetriesExhausted") {
                saw = true;
            }
        }
        if saw {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(
        saw,
        "expected OnFailure destination record on SNS for async invoke that errored"
    );
}
