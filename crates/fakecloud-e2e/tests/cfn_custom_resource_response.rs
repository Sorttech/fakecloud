//! Custom resources signal their outcome by PUT to the event's `ResponseURL`.
//! fakecloud sent no such field, so `cfn-response` handlers (every CDK one)
//! threw on `new URL(undefined)` and nothing ever waited for a signal.

mod helpers;

use helpers::TestServer;

/// The endpoint `cfn-response` PUTs to. It sends no auth and expects a plain
/// 200; anything else makes the handler retry or throw.
#[tokio::test]
async fn custom_resource_response_endpoint_accepts_a_signal() {
    let server = TestServer::start().await;
    let client = reqwest::Client::new();
    let url = format!(
        "{}/_fakecloud/cfn/custom-resource-response/req-abc",
        server.endpoint()
    );

    let ok = client
        .put(&url)
        .body(r#"{"Status":"SUCCESS","PhysicalResourceId":"p-1"}"#)
        .send()
        .await
        .expect("signal PUT sends");
    assert_eq!(ok.status(), 200);

    // A body that is not a signal must be rejected rather than recorded: a
    // stray PUT cannot be allowed to decide an unrelated resource's fate.
    let bad = client
        .put(&url)
        .body(r#"{"nope":true}"#)
        .send()
        .await
        .expect("PUT sends");
    assert_eq!(bad.status(), 400);
}
