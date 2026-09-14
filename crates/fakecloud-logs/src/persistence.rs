//! Bounded-size append-only event segments with an atomic metadata manifest.
//! A manifest commits byte lengths only after event files are synced. Recovery
//! ignores uncommitted tails; the next writer truncates them before appending.
use std::collections::{BTreeMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::Arc;

use crate::state::{LogEvent, LogsSnapshot, LogsState, LOGS_SNAPSHOT_SCHEMA_VERSION};
use fakecloud_core::multi_account::MultiAccountState;
use fakecloud_persistence::{atomic::write_atomic_bytes, SnapshotStore};
use serde::{Deserialize, Serialize};

type Accounts = MultiAccountState<LogsState>;
const SEGMENT_BYTES: u64 = 4 * 1024 * 1024;

pub trait LogsStore: Send + Sync {
    fn load(&self) -> io::Result<Option<LogsSnapshot>>;
    fn save(&self, state: &mut Accounts) -> io::Result<()>;
}

/// Compatibility adapter for embedders using the original opaque store API.
pub struct SnapshotLogsStore(pub Arc<dyn SnapshotStore>);
impl LogsStore for SnapshotLogsStore {
    fn load(&self) -> io::Result<Option<LogsSnapshot>> {
        self.0
            .load()?
            .map(|b| serde_json::from_slice(&b).map_err(invalid))
            .transpose()
    }
    fn save(&self, state: &mut Accounts) -> io::Result<()> {
        prune_expired(state, chrono::Utc::now().timestamp_millis());
        #[derive(Serialize)]
        struct Snapshot<'a> {
            schema_version: u32,
            accounts: &'a Accounts,
            state: Option<()>,
        }
        self.0.save(
            &serde_json::to_vec(&Snapshot {
                schema_version: LOGS_SNAPSHOT_SCHEMA_VERSION,
                accounts: state,
                state: None,
            })
            .map_err(invalid)?,
        )
    }
}

#[derive(Serialize, Deserialize)]
struct Manifest {
    version: u32,
    metadata: LogsSnapshot,
    streams: BTreeMap<String, StreamFiles>,
}
#[derive(Clone, Default, Serialize, Deserialize)]
struct StreamFiles {
    max_sequence: u64,
    segments: Vec<Segment>,
}
#[derive(Clone, Serialize, Deserialize)]
struct Segment {
    file: String,
    length: u64,
    max_timestamp: i64,
    #[serde(default)]
    expired_before: Option<i64>,
}

pub struct SegmentedLogsStore {
    directory: PathBuf,
}
impl SegmentedLogsStore {
    pub fn new(directory: PathBuf) -> Self {
        Self { directory }
    }

    fn read_manifest(&self) -> io::Result<Option<Manifest>> {
        let file = match File::open(self.directory.join("manifest.json")) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let manifest: Manifest = serde_json::from_reader(BufReader::new(file)).map_err(invalid)?;
        if manifest.version != 1 || manifest.metadata.schema_version > LOGS_SNAPSHOT_SCHEMA_VERSION
        {
            return Err(invalid("unsupported Logs manifest version"));
        }
        // Filenames are generated UUIDs, never API-provided group/stream names.
        for files in manifest.streams.values() {
            for segment in &files.segments {
                if !segment.file.ends_with(".jsonl")
                    || uuid::Uuid::parse_str(segment.file.trim_end_matches(".jsonl")).is_err()
                {
                    return Err(invalid("invalid Logs segment filename"));
                }
            }
        }
        Ok(Some(manifest))
    }

    fn append_events<'a>(
        &self,
        files: &mut StreamFiles,
        events: impl Iterator<Item = &'a LogEvent>,
    ) -> io::Result<()> {
        let mut writer: Option<BufWriter<File>> = None;
        for event in events {
            let rotate = files.segments.last().is_none_or(|s| {
                s.length >= SEGMENT_BYTES
                    || s.expired_before
                        .is_some_and(|cutoff| event.timestamp < cutoff)
            });
            if rotate {
                if let Some(mut writer) = writer.take() {
                    writer.flush()?;
                    writer.get_ref().sync_all()?;
                }
                files.segments.push(Segment {
                    file: format!("{}.jsonl", uuid::Uuid::new_v4()),
                    length: 0,
                    max_timestamp: i64::MIN,
                    expired_before: None,
                });
            }
            let segment = files.segments.last_mut().expect("segment created");
            if writer.is_none() {
                let path = self.directory.join(&segment.file);
                let mut file = OpenOptions::new()
                    .create(true)
                    .truncate(false)
                    .read(true)
                    .write(true)
                    .open(path)?;
                if file.metadata()?.len() < segment.length {
                    return Err(invalid("truncated committed Logs segment"));
                }
                file.set_len(segment.length)?;
                file.seek(SeekFrom::Start(segment.length))?;
                writer = Some(BufWriter::new(file));
            }
            // At most one event-sized temporary buffer, never historical state.
            let bytes = serde_json::to_vec(event).map_err(invalid)?;
            let writer = writer.as_mut().expect("writer opened");
            writer.write_all(&bytes)?;
            writer.write_all(b"\n")?;
            segment.length += bytes.len() as u64 + 1;
            segment.max_timestamp = segment.max_timestamp.max(event.timestamp);
            files.max_sequence = files.max_sequence.max(event.seq);
        }
        if let Some(mut writer) = writer {
            writer.flush()?;
            writer.get_ref().sync_all()?;
        }
        Ok(())
    }

    fn collect_unreferenced(&self, manifest: &Manifest) -> io::Result<()> {
        let live: HashSet<&str> = manifest
            .streams
            .values()
            .flat_map(|s| &s.segments)
            .map(|s| s.file.as_str())
            .collect();
        for entry in std::fs::read_dir(&self.directory)? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.ends_with(".jsonl")
                && uuid::Uuid::parse_str(name.trim_end_matches(".jsonl")).is_ok()
                && !live.contains(name.as_ref())
            {
                std::fs::remove_file(entry.path())?;
            }
        }
        Ok(())
    }
}

impl LogsStore for SegmentedLogsStore {
    fn load(&self) -> io::Result<Option<LogsSnapshot>> {
        let Some(mut manifest) = self.read_manifest()? else {
            return match File::open(self.directory.join("snapshot.json")) {
                Ok(file) => Ok(Some(
                    serde_json::from_reader(BufReader::new(file)).map_err(invalid)?,
                )),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e),
            };
        };
        let accounts = manifest
            .metadata
            .accounts
            .as_mut()
            .ok_or_else(|| invalid("missing Logs accounts"))?;
        for (_, account) in accounts.iter_mut() {
            for group in account.log_groups.values_mut() {
                for stream in group.log_streams.values_mut() {
                    if !stream.events.is_empty() {
                        return Err(invalid("events in Logs metadata"));
                    }
                    let files = manifest
                        .streams
                        .get(&stream.persistence_id)
                        .ok_or_else(|| invalid("missing Logs stream segments"))?;
                    for segment in &files.segments {
                        let file = File::open(self.directory.join(&segment.file))?;
                        if file.metadata()?.len() < segment.length {
                            return Err(invalid("truncated committed Logs segment"));
                        }
                        let mut reader = BufReader::new(file.take(segment.length));
                        let mut line = String::new();
                        while reader.read_line(&mut line)? != 0 {
                            if !line.ends_with('\n') {
                                return Err(invalid("incomplete committed Logs event"));
                            }
                            let event: LogEvent = serde_json::from_str(&line).map_err(invalid)?;
                            if segment
                                .expired_before
                                .is_none_or(|cutoff| event.timestamp >= cutoff)
                            {
                                stream.events.push(event);
                            }
                            line.clear();
                        }
                    }
                    stream.events.sort_by_key(|e| (e.timestamp, e.seq));
                }
            }
        }
        // Commit any expiration applied during recovery before a caller can
        // remove/extend the retention policy and accidentally resurrect rows.
        self.save(accounts)?;
        Ok(Some(manifest.metadata))
    }

    fn save(&self, state: &mut Accounts) -> io::Result<()> {
        let now = chrono::Utc::now().timestamp_millis();
        prune_expired(state, now);
        std::fs::create_dir_all(&self.directory)?;
        let previous = self.read_manifest()?;
        let mut streams = BTreeMap::new();
        for (_, account) in state.iter_mut() {
            for group in account.log_groups.values_mut() {
                let cutoff = group
                    .retention_in_days
                    .map(|days| now.saturating_sub(i64::from(days) * 86_400_000));
                for stream in group.log_streams.values_mut() {
                    let mut files = previous
                        .as_ref()
                        .and_then(|m| m.streams.get(&stream.persistence_id))
                        .cloned()
                        .unwrap_or_default();
                    if let Some(cutoff) = cutoff {
                        files.segments.retain(|s| s.max_timestamp >= cutoff);
                        for segment in &mut files.segments {
                            segment.expired_before =
                                Some(segment.expired_before.unwrap_or(i64::MIN).max(cutoff));
                        }
                    }
                    // v1 snapshots may contain seq=0 or repeated IDs. Assign stable
                    // IDs once during migration; modern snapshots retain their IDs.
                    if previous.is_none() {
                        let mut seen = HashSet::new();
                        for event in &mut stream.events {
                            if event.seq == 0 || !seen.insert(event.seq) {
                                stream.last_sequence += 1;
                                event.seq = stream.last_sequence;
                                seen.insert(event.seq);
                            }
                        }
                    }
                    let watermark = files.max_sequence;
                    self.append_events(
                        &mut files,
                        stream.events.iter().filter(|event| event.seq > watermark),
                    )?;
                    streams.insert(stream.persistence_id.clone(), files);
                }
            }
        }
        let manifest = Manifest {
            version: 1,
            metadata: LogsSnapshot {
                schema_version: LOGS_SNAPSHOT_SCHEMA_VERSION,
                accounts: Some(state.map(LogsState::metadata)),
                state: None,
            },
            streams,
        };
        // Sync directory entries for new segments before committing references.
        File::open(&self.directory)?.sync_all()?;
        write_atomic_bytes(
            &self.directory.join("manifest.json"),
            &serde_json::to_vec(&manifest).map_err(invalid)?,
        )?;
        // Once the manifest is durable, the old whole-state file is obsolete.
        // Keeping it would retain data deleted by retention indefinitely.
        if let Err(error) = std::fs::remove_file(self.directory.join("snapshot.json")) {
            if error.kind() != io::ErrorKind::NotFound {
                tracing::warn!(%error, "failed to remove migrated Logs snapshot");
            }
        }
        // Cleanup is after commit: crashes can leak files, never committed data.
        if let Err(e) = self.collect_unreferenced(&manifest) {
            tracing::warn!(error = %e, "failed to reclaim unreferenced Logs segments");
        }
        Ok(())
    }
}

/// Retention is a storage policy, not merely a query filter.
pub fn prune_expired(state: &mut Accounts, now: i64) {
    for (_, account) in state.iter_mut() {
        for group in account.log_groups.values_mut() {
            let cutoff = group
                .retention_in_days
                .map(|days| now.saturating_sub(i64::from(days) * 86_400_000));
            for stream in group.log_streams.values_mut() {
                stream.last_sequence = stream
                    .last_sequence
                    .max(stream.events.iter().map(|e| e.seq).max().unwrap_or(0));
                if let Some(cutoff) = cutoff {
                    let before = stream.events.len();

                    stream.events.retain(|e| e.timestamp >= cutoff);

                    if stream.events.len() < before {
                        stream.events.shrink_to_fit();
                    }
                }
            }
            group.stored_bytes = group
                .log_streams
                .values()
                .flat_map(|stream| &stream.events)
                .map(|event| event.message.len() as i64 + 26)
                .sum();
        }
    }
}
fn invalid(error: impl ToString) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}

#[cfg(test)]
#[path = "persistence_tests.rs"]
mod tests;
