use super::*;
use crate::ingest::{append_events, IngestEvent};
use crate::state::SharedLogsState;
use parking_lot::RwLock;
use std::sync::Arc;

fn state() -> SharedLogsState {
    Arc::new(RwLock::new(Accounts::new(
        "a",
        "us-east-1",
        "http://localhost",
    )))
}
fn put(state: &SharedLogsState, account: &str, timestamp: i64, message: &str) {
    append_events(
        state,
        account,
        "us-east-1",
        "g",
        "s",
        &[IngestEvent {
            timestamp_ms: timestamp,
            message: message.into(),
        }],
    );
}
fn events(snapshot: &LogsSnapshot, account: &str) -> Vec<String> {
    snapshot
        .accounts
        .as_ref()
        .unwrap()
        .get(account)
        .unwrap()
        .log_groups["g"]
        .log_streams["s"]
        .events
        .iter()
        .map(|e| e.message.clone())
        .collect()
}
#[test]
fn appending_does_not_rewrite_old_events_and_restores_sorted_events() {
    let dir = tempfile::tempdir().unwrap();
    let store = SegmentedLogsStore::new(dir.path().into());
    let state = state();
    let now = chrono::Utc::now().timestamp_millis();
    put(&state, "a", now, &"x".repeat(100_000));
    store.save(&mut state.write()).unwrap();
    let manifest = store.read_manifest().unwrap().unwrap();
    let segment = &manifest.streams.values().next().unwrap().segments[0];
    let file = dir.path().join(&segment.file);
    let old_bytes = std::fs::read(&file).unwrap();
    put(&state, "a", now - 1, "earlier");
    store.save(&mut state.write()).unwrap();
    let bytes = std::fs::read(&file).unwrap();
    assert!(bytes.starts_with(&old_bytes));
    assert!(bytes.len() - old_bytes.len() < 512);
    let restored = store.load().unwrap().unwrap();
    assert_eq!(
        events(&restored, "a"),
        vec!["earlier".to_string(), "x".repeat(100_000)]
    );
}
#[test]
fn ignores_and_truncates_uncommitted_tail() {
    let dir = tempfile::tempdir().unwrap();
    let store = SegmentedLogsStore::new(dir.path().into());
    let state = state();
    put(&state, "a", 100, "committed");
    store.save(&mut state.write()).unwrap();
    let manifest = store.read_manifest().unwrap().unwrap();
    let segment = &manifest.streams.values().next().unwrap().segments[0];
    let path = dir.path().join(&segment.file);
    OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"partial garbage")
        .unwrap();
    assert_eq!(
        events(&store.load().unwrap().unwrap(), "a"),
        vec!["committed"]
    );
    put(&state, "a", 200, "next");
    store.save(&mut state.write()).unwrap();
    assert_eq!(
        events(&store.load().unwrap().unwrap(), "a"),
        vec!["committed", "next"]
    );
}
#[test]
fn migrates_legacy_snapshot_and_isolates_accounts() {
    let dir = tempfile::tempdir().unwrap();
    let state = state();
    put(&state, "a", 100, "a-event");
    put(&state, "b", 100, "b-event");
    let old = LogsSnapshot {
        schema_version: 2,
        accounts: Some(state.read().clone()),
        state: None,
    };
    std::fs::write(
        dir.path().join("snapshot.json"),
        serde_json::to_vec(&old).unwrap(),
    )
    .unwrap();
    let store = SegmentedLogsStore::new(dir.path().into());
    let mut loaded = store.load().unwrap().unwrap().accounts.unwrap();
    store.save(&mut loaded).unwrap();
    assert_eq!(
        events(&store.load().unwrap().unwrap(), "a"),
        vec!["a-event"]
    );
    assert_eq!(
        events(&store.load().unwrap().unwrap(), "b"),
        vec!["b-event"]
    );
}
#[test]
fn retention_reclaims_memory_and_expired_segments_without_reusing_sequences() {
    let dir = tempfile::tempdir().unwrap();
    let store = SegmentedLogsStore::new(dir.path().into());
    let state = state();
    let now = chrono::Utc::now().timestamp_millis();
    put(&state, "a", now - 2 * 86_400_000, "expired");
    store.save(&mut state.write()).unwrap();
    let old_seq = state.read().get("a").unwrap().log_groups["g"].log_streams["s"].events[0].seq;
    let old_file = store
        .read_manifest()
        .unwrap()
        .unwrap()
        .streams
        .values()
        .next()
        .unwrap()
        .segments[0]
        .file
        .clone();
    state
        .write()
        .get_mut("a")
        .unwrap()
        .log_groups
        .get_mut("g")
        .unwrap()
        .retention_in_days = Some(1);
    store.save(&mut state.write()).unwrap();
    assert!(
        state.read().get("a").unwrap().log_groups["g"].log_streams["s"]
            .events
            .is_empty()
    );
    assert!(!dir.path().join(old_file).exists());
    put(&state, "a", now, "fresh");
    assert!(
        state.read().get("a").unwrap().log_groups["g"].log_streams["s"].events[0].seq > old_seq
    );
    store.save(&mut state.write()).unwrap();
    assert_eq!(events(&store.load().unwrap().unwrap(), "a"), vec!["fresh"]);
}
#[test]
fn deleted_and_recreated_stream_does_not_resurrect_events() {
    let dir = tempfile::tempdir().unwrap();
    let store = SegmentedLogsStore::new(dir.path().into());
    let state = state();
    put(&state, "a", 100, "old");
    store.save(&mut state.write()).unwrap();
    state
        .write()
        .get_mut("a")
        .unwrap()
        .log_groups
        .get_mut("g")
        .unwrap()
        .log_streams
        .clear();
    put(&state, "a", 100, "replacement");
    store.save(&mut state.write()).unwrap();
    assert_eq!(
        events(&store.load().unwrap().unwrap(), "a"),
        vec!["replacement"]
    );
}

#[test]
fn removing_retention_never_resurrects_deleted_rows_but_accepts_new_late_events() {
    let dir = tempfile::tempdir().unwrap();
    let store = SegmentedLogsStore::new(dir.path().into());
    let state = state();
    let now = chrono::Utc::now().timestamp_millis();
    let old = now - 2 * 86_400_000;
    put(&state, "a", old, "deleted");
    put(&state, "a", now, "live");
    store.save(&mut state.write()).unwrap();
    state
        .write()
        .get_mut("a")
        .unwrap()
        .log_groups
        .get_mut("g")
        .unwrap()
        .retention_in_days = Some(1);
    store.save(&mut state.write()).unwrap();
    for _ in 0..2 {
        let restored = store.load().unwrap().unwrap();
        let accounts = restored.accounts.as_ref().unwrap();
        assert_eq!(accounts.get("a").unwrap().log_groups["g"].stored_bytes, 30);
        assert_eq!(events(&restored, "a"), vec!["live"]);
    }
    state
        .write()
        .get_mut("a")
        .unwrap()
        .log_groups
        .get_mut("g")
        .unwrap()
        .retention_in_days = None;
    put(&state, "a", old, "new-late");
    store.save(&mut state.write()).unwrap();
    assert_eq!(
        events(&store.load().unwrap().unwrap(), "a"),
        vec!["new-late", "live"]
    );
}

#[test]
fn sealed_segment_is_not_rewritten_when_next_segment_is_appended() {
    let dir = tempfile::tempdir().unwrap();
    let store = SegmentedLogsStore::new(dir.path().into());
    let state = state();
    for i in 0..5 {
        put(&state, "a", i, &"x".repeat(1024 * 1024));
    }
    store.save(&mut state.write()).unwrap();
    let manifest = store.read_manifest().unwrap().unwrap();
    let files = manifest.streams.values().next().unwrap();
    assert!(files.segments.len() >= 2);
    let path = dir.path().join(&files.segments[0].file);
    let before = std::fs::metadata(&path).unwrap().modified().unwrap();
    put(&state, "a", 6, "new");
    store.save(&mut state.write()).unwrap();
    assert_eq!(std::fs::metadata(path).unwrap().modified().unwrap(), before);
}

#[test]
fn rejects_truncated_committed_data_instead_of_silently_losing_events() {
    let dir = tempfile::tempdir().unwrap();
    let store = SegmentedLogsStore::new(dir.path().into());
    let state = state();
    put(&state, "a", 1, "committed");
    store.save(&mut state.write()).unwrap();
    let manifest = store.read_manifest().unwrap().unwrap();
    let segment = &manifest.streams.values().next().unwrap().segments[0];
    OpenOptions::new()
        .write(true)
        .open(dir.path().join(&segment.file))
        .unwrap()
        .set_len(2)
        .unwrap();
    assert!(store.load().is_err());
    put(&state, "a", 2, "next");
    assert!(store.save(&mut state.write()).is_err());
}

#[test]
fn expiration_during_downtime_is_committed_before_policy_removal() {
    let dir = tempfile::tempdir().unwrap();
    let store = SegmentedLogsStore::new(dir.path().into());
    let state = state();
    let now = chrono::Utc::now().timestamp_millis();
    put(&state, "a", now - 2 * 86_400_000, "expired-offline");
    put(&state, "a", now, "live");
    store.save(&mut state.write()).unwrap();
    // Simulate an older manifest whose one-day policy has not yet swept
    // these events, without relying on a wall-clock sleep.
    let mut manifest = store.read_manifest().unwrap().unwrap();
    manifest
        .metadata
        .accounts
        .as_mut()
        .unwrap()
        .get_mut("a")
        .unwrap()
        .log_groups
        .get_mut("g")
        .unwrap()
        .retention_in_days = Some(1);
    std::fs::write(
        dir.path().join("manifest.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    let restored = store.load().unwrap().unwrap();
    assert_eq!(events(&restored, "a"), vec!["live"]);
    let mut accounts = restored.accounts.unwrap();
    accounts
        .get_mut("a")
        .unwrap()
        .log_groups
        .get_mut("g")
        .unwrap()
        .retention_in_days = None;
    store.save(&mut accounts).unwrap();
    assert_eq!(events(&store.load().unwrap().unwrap(), "a"), vec!["live"]);
}

#[test]
fn migrates_v1_snapshot_without_sequence_or_persistence_fields() {
    let dir = tempfile::tempdir().unwrap();
    let state = state();
    put(&state, "a", 100, "first");
    put(&state, "a", 200, "second");
    let mut single = serde_json::to_value(state.read().get("a").unwrap()).unwrap();
    let stream = single["log_groups"]["g"]["log_streams"]["s"]
        .as_object_mut()
        .unwrap();
    stream.remove("persistence_id");
    stream.remove("last_sequence");
    for event in stream.get_mut("events").unwrap().as_array_mut().unwrap() {
        event.as_object_mut().unwrap().remove("seq");
    }
    std::fs::write(
        dir.path().join("snapshot.json"),
        serde_json::to_vec(&serde_json::json!({"schema_version": 1, "state": single})).unwrap(),
    )
    .unwrap();
    let store = SegmentedLogsStore::new(dir.path().into());
    let loaded = store.load().unwrap().unwrap().state.unwrap();
    let mut accounts = Accounts::new("a", "us-east-1", "http://localhost");
    *accounts.get_or_create("a") = loaded;
    store.save(&mut accounts).unwrap();
    assert!(!dir.path().join("snapshot.json").exists());
    let restored = store.load().unwrap().unwrap();
    assert_eq!(events(&restored, "a"), vec!["first", "second"]);
    let rows = &restored
        .accounts
        .as_ref()
        .unwrap()
        .get("a")
        .unwrap()
        .log_groups["g"]
        .log_streams["s"]
        .events;
    assert!(rows[0].seq > 0);
    assert!(rows[1].seq > rows[0].seq);
}
