mod helpers;

use aws_sdk_cloudwatchlogs::types::InputLogEvent;
use helpers::TestServer;

/// Log group, retention, tags, and log streams + events survive restart.
#[tokio::test]
async fn persistence_round_trip_group_stream_and_events() {
    let tmp = tempfile::tempdir().unwrap();
    let mut server = TestServer::start_persistent(tmp.path()).await;
    let logs = server.logs_client().await;

    logs.create_log_group()
        .log_group_name("/app/web")
        .send()
        .await
        .unwrap();
    logs.put_retention_policy()
        .log_group_name("/app/web")
        .retention_in_days(7)
        .send()
        .await
        .unwrap();
    logs.tag_resource()
        .resource_arn("arn:aws:logs:us-east-1:123456789012:log-group:/app/web")
        .tags("env", "prod")
        .send()
        .await
        .unwrap();

    logs.create_log_stream()
        .log_group_name("/app/web")
        .log_stream_name("instance-1")
        .send()
        .await
        .unwrap();

    let now = chrono::Utc::now().timestamp_millis();
    logs.put_log_events()
        .log_group_name("/app/web")
        .log_stream_name("instance-1")
        .log_events(
            InputLogEvent::builder()
                .timestamp(now)
                .message("hello persistence")
                .build()
                .unwrap(),
        )
        .log_events(
            InputLogEvent::builder()
                .timestamp(now + 1)
                .message("second line")
                .build()
                .unwrap(),
        )
        .send()
        .await
        .unwrap();

    server.restart().await;
    let logs = server.logs_client().await;

    // Log group survives with retention.
    let groups = logs
        .describe_log_groups()
        .log_group_name_prefix("/app/web")
        .send()
        .await
        .unwrap();
    let g = groups
        .log_groups()
        .iter()
        .find(|g| g.log_group_name() == Some("/app/web"))
        .unwrap();
    assert_eq!(g.retention_in_days(), Some(7));

    // Stream survives.
    let streams = logs
        .describe_log_streams()
        .log_group_name("/app/web")
        .send()
        .await
        .unwrap();
    assert!(streams
        .log_streams()
        .iter()
        .any(|s| s.log_stream_name() == Some("instance-1")));

    // Events survive in order.
    let events = logs
        .get_log_events()
        .log_group_name("/app/web")
        .log_stream_name("instance-1")
        .start_from_head(true)
        .send()
        .await
        .unwrap();
    let msgs: Vec<&str> = events.events().iter().filter_map(|e| e.message()).collect();
    assert_eq!(msgs, vec!["hello persistence", "second line"]);
}

/// Subscription filters and delete-group durability.
#[tokio::test]
async fn persistence_subscription_filter_and_delete_survive_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let mut server = TestServer::start_persistent(tmp.path()).await;
    let logs = server.logs_client().await;

    logs.create_log_group()
        .log_group_name("/app/keeper")
        .send()
        .await
        .unwrap();
    logs.create_log_group()
        .log_group_name("/app/goner")
        .send()
        .await
        .unwrap();

    logs.put_subscription_filter()
        .log_group_name("/app/keeper")
        .filter_name("errs")
        .filter_pattern("ERROR")
        .destination_arn("arn:aws:lambda:us-east-1:123456789012:function:notify")
        .send()
        .await
        .unwrap();

    logs.delete_log_group()
        .log_group_name("/app/goner")
        .send()
        .await
        .unwrap();

    server.restart().await;
    let logs = server.logs_client().await;

    // Keeper and its filter survive.
    let filters = logs
        .describe_subscription_filters()
        .log_group_name("/app/keeper")
        .send()
        .await
        .unwrap();
    assert!(filters
        .subscription_filters()
        .iter()
        .any(|f| f.filter_name() == Some("errs") && f.filter_pattern() == Some("ERROR")));

    // Goner is gone.
    let groups = logs
        .describe_log_groups()
        .log_group_name_prefix("/app/goner")
        .send()
        .await
        .unwrap();
    assert!(groups.log_groups().is_empty());
}

/// Retention removes stored events permanently, even when the policy is later
/// deleted. A newly ingested late event is still accepted after policy removal.
#[tokio::test]
async fn segmented_retention_does_not_resurrect_expired_events() {
    let tmp = tempfile::tempdir().unwrap();
    let mut server = TestServer::start_persistent(tmp.path()).await;
    let logs = server.logs_client().await;
    logs.create_log_group()
        .log_group_name("retained")
        .send()
        .await
        .unwrap();
    logs.create_log_stream()
        .log_group_name("retained")
        .log_stream_name("s")
        .send()
        .await
        .unwrap();
    let now = chrono::Utc::now().timestamp_millis();
    let old = now - 2 * 86_400_000;
    for (timestamp, message) in [(old, "expired"), (now, "live")] {
        logs.put_log_events()
            .log_group_name("retained")
            .log_stream_name("s")
            .log_events(
                InputLogEvent::builder()
                    .timestamp(timestamp)
                    .message(message)
                    .build()
                    .unwrap(),
            )
            .send()
            .await
            .unwrap();
    }
    logs.put_retention_policy()
        .log_group_name("retained")
        .retention_in_days(1)
        .send()
        .await
        .unwrap();
    logs.delete_retention_policy()
        .log_group_name("retained")
        .send()
        .await
        .unwrap();
    logs.put_log_events()
        .log_group_name("retained")
        .log_stream_name("s")
        .log_events(
            InputLogEvent::builder()
                .timestamp(old)
                .message("new-late")
                .build()
                .unwrap(),
        )
        .send()
        .await
        .unwrap();
    assert!(tmp.path().join("logs/manifest.json").exists());
    server.restart().await;
    let response = server
        .logs_client()
        .await
        .get_log_events()
        .log_group_name("retained")
        .log_stream_name("s")
        .start_from_head(true)
        .send()
        .await
        .unwrap();
    let messages: Vec<_> = response
        .events()
        .iter()
        .filter_map(|e| e.message())
        .collect();
    assert_eq!(messages, vec!["new-late", "live"]);
}
