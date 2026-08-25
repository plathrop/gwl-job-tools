//! Append-only JSONL event store (design doc 0001 §1).
//!
//! - Single writer: an exclusive `flock` on `<log>.lock` is held for the
//!   store's lifetime; a second process fails fast.
//! - Batch append = commit unit: the whole batch is serialized and written
//!   with one `write_all` on an `O_APPEND` file, then `fsync`.
//! - Replay tolerates a torn trailing line (crash mid-write) with a warning;
//!   any other malformed line is a hard error.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use fs2::FileExt;
use jiff::Timestamp;
use miette::{Context, IntoDiagnostic, Result, bail, miette};
use tracing::{instrument, warn};
use uuid::Uuid;

use crate::domain::events::{ENVELOPE_VERSION, EventEnvelope, PendingEvent};
use crate::event_store::EventStore;
use crate::event_store::upcast::upcast;

pub struct JsonlEventStore {
    path: PathBuf,
    /// Held for the store's lifetime; dropping releases the flock.
    _lock: File,
}

impl JsonlEventStore {
    /// Open (creating if necessary) the log at `path` and take the
    /// single-writer lock on `<path>.lock`.
    #[instrument(skip_all)]
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .into_diagnostic()
                .wrap_err_with(|| format!("creating data dir {}", parent.display()))?;
        }
        // Touch the log so it always exists.
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .into_diagnostic()
            .wrap_err_with(|| format!("opening event log {}", path.display()))?;

        let lock_path = path.with_extension("lock");
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .into_diagnostic()
            .wrap_err_with(|| format!("opening lock file {}", lock_path.display()))?;
        lock.try_lock_exclusive().map_err(|_| {
            miette!(
                "another gwl-jobs process holds the event log lock ({}); \
                 only one writer at a time is allowed",
                lock_path.display()
            )
        })?;

        Ok(Self { path, _lock: lock })
    }

    fn read_envelopes(&self) -> Result<Vec<EventEnvelope>> {
        let file = File::open(&self.path)
            .into_diagnostic()
            .wrap_err_with(|| format!("reading event log {}", self.path.display()))?;
        let mut envelopes = Vec::new();
        let lines: Vec<String> = BufReader::new(file)
            .lines()
            .collect::<std::io::Result<Vec<_>>>()
            .into_diagnostic()?;

        let line_count = lines.len();
        for (idx, line) in lines.into_iter().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            match parse_envelope(&line) {
                Ok(envelope) => envelopes.push(envelope),
                Err(err) => {
                    // Torn tail: a crash mid-write can leave a partial final
                    // line. Discard it with a warning; anything else is
                    // corruption and a hard error (design doc §1).
                    let is_last = idx == line_count - 1;
                    if is_last {
                        warn!(
                            line = idx + 1,
                            error = %err,
                            "discarding torn trailing line in event log (crash mid-write?)"
                        );
                    } else {
                        return Err(err).wrap_err_with(|| {
                            format!(
                                "malformed event at {} line {} (not a torn tail; \
                                 refusing to replay a corrupt log)",
                                self.path.display(),
                                idx + 1
                            )
                        });
                    }
                }
            }
        }
        Ok(envelopes)
    }
}

fn parse_envelope(line: &str) -> Result<EventEnvelope> {
    let mut envelope: EventEnvelope = serde_json::from_str(line).into_diagnostic()?;
    envelope.payload = upcast(
        &envelope.event_type,
        envelope.schema_version,
        envelope.payload.take(),
    )?;
    if envelope.envelope_version != ENVELOPE_VERSION {
        bail!(
            "unsupported envelope_version {} (this build understands {ENVELOPE_VERSION})",
            envelope.envelope_version
        );
    }
    Ok(envelope)
}

impl EventStore for JsonlEventStore {
    #[instrument(skip(self, events), fields(stream, count = events.len()))]
    fn append(
        &mut self,
        stream: &str,
        expected_seq: u64,
        events: &[PendingEvent],
        correlation_id: Uuid,
    ) -> Result<Vec<EventEnvelope>> {
        // Optimistic concurrency: re-read the stream and verify the
        // caller's view is current. We hold the single-writer lock, so the
        // log cannot change between this check and the write.
        let current_seq = self.load(stream)?.last().map(|e| e.seq).unwrap_or(0);
        if current_seq != expected_seq {
            bail!(
                "concurrent modification of stream '{stream}': expected seq \
                 {expected_seq}, found {current_seq}"
            );
        }

        let now = Timestamp::now();
        let mut batch = String::new();
        let mut envelopes = Vec::with_capacity(events.len());
        for (i, pending) in events.iter().enumerate() {
            let envelope = EventEnvelope {
                envelope_version: ENVELOPE_VERSION,
                id: Uuid::now_v7(),
                stream: stream.to_string(),
                seq: expected_seq + 1 + i as u64,
                event_type: pending.event_type.to_string(),
                schema_version: pending.schema_version,
                occurred_at: now,
                recorded_at: now,
                causation_id: pending.causation_id,
                correlation_id,
                payload: pending.payload.clone(),
            };
            batch.push_str(&serde_json::to_string(&envelope).into_diagnostic()?);
            batch.push('\n');
            envelopes.push(envelope);
        }

        if !batch.is_empty() {
            // One write for the whole batch (the commit unit), then fsync.
            let mut file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)
                .into_diagnostic()?;
            file.write_all(batch.as_bytes()).into_diagnostic()?;
            file.sync_all().into_diagnostic()?;
        }

        Ok(envelopes)
    }

    fn load(&self, stream: &str) -> Result<Vec<EventEnvelope>> {
        Ok(self
            .read_envelopes()?
            .into_iter()
            .filter(|e| e.stream == stream)
            .collect())
    }

    fn replay(&self) -> Result<Vec<EventEnvelope>> {
        self.read_envelopes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::events::event_type;

    fn pending(payload: serde_json::Value) -> PendingEvent {
        PendingEvent {
            event_type: event_type::INGESTED,
            schema_version: 1,
            causation_id: None,
            payload,
        }
    }

    fn store_in(dir: &tempfile::TempDir) -> JsonlEventStore {
        JsonlEventStore::open(dir.path().join("events.jsonl")).unwrap()
    }

    // ── append / replay round-trip ───────────────────────────────

    #[test]
    fn append_then_replay_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = store_in(&dir);
        let correlation = Uuid::now_v7();

        let written = store
            .append(
                "lead/00000000-0000-7000-8000-000000000001",
                0,
                &[pending(serde_json::json!({"dedupe_key": "tc:abc"}))],
                correlation,
            )
            .unwrap();
        assert_eq!(written.len(), 1);
        assert_eq!(written[0].seq, 1);
        assert_eq!(written[0].correlation_id, correlation);

        let replayed = store.replay().unwrap();
        assert_eq!(replayed.len(), 1);
        assert_eq!(replayed[0].payload["dedupe_key"], "tc:abc");
    }

    #[test]
    fn batch_append_numbers_seqs_consecutively() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = store_in(&dir);
        let stream = "lead/00000000-0000-7000-8000-000000000002";
        store
            .append(
                stream,
                0,
                &[
                    pending(serde_json::json!({"n": 1})),
                    pending(serde_json::json!({"n": 2})),
                ],
                Uuid::now_v7(),
            )
            .unwrap();
        let seqs: Vec<u64> = store.load(stream).unwrap().iter().map(|e| e.seq).collect();
        assert_eq!(seqs, vec![1, 2]);

        // Continuing the stream requires the current seq.
        store
            .append(
                stream,
                2,
                &[pending(serde_json::json!({"n": 3}))],
                Uuid::now_v7(),
            )
            .unwrap();
        assert_eq!(store.load(stream).unwrap().last().unwrap().seq, 3);
    }

    #[test]
    fn stale_expected_seq_bails() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = store_in(&dir);
        let stream = "lead/00000000-0000-7000-8000-000000000003";
        store
            .append(stream, 0, &[pending(serde_json::json!({}))], Uuid::now_v7())
            .unwrap();
        let result = store.append(stream, 0, &[pending(serde_json::json!({}))], Uuid::now_v7());
        assert!(result.is_err());
    }

    // ── torn tail tolerance ──────────────────────────────────────

    #[test]
    fn torn_trailing_line_is_discarded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let mut store = JsonlEventStore::open(&path).unwrap();
        store
            .append(
                "lead/00000000-0000-7000-8000-000000000004",
                0,
                &[pending(serde_json::json!({"ok": true}))],
                Uuid::now_v7(),
            )
            .unwrap();

        // Simulate a crash mid-write: partial JSON, no newline.
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"{\"envelope_version\":1,\"id\":").unwrap();
        drop(file);

        let replayed = store.replay().unwrap();
        assert_eq!(replayed.len(), 1);
        assert_eq!(replayed[0].payload["ok"], true);
    }

    #[test]
    fn corrupt_middle_line_is_hard_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let mut store = JsonlEventStore::open(&path).unwrap();
        store
            .append(
                "lead/00000000-0000-7000-8000-000000000005",
                0,
                &[pending(serde_json::json!({}))],
                Uuid::now_v7(),
            )
            .unwrap();

        // Corrupt line followed by a valid one (not a torn tail).
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"not json\n").unwrap();
        drop(file);
        store
            .append(
                "lead/00000000-0000-7000-8000-000000000005",
                1,
                &[pending(serde_json::json!({}))],
                Uuid::now_v7(),
            )
            .unwrap();

        assert!(store.replay().is_err());
    }

    // ── single-writer lock ───────────────────────────────────────

    #[test]
    fn second_open_fails_fast() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let _first = JsonlEventStore::open(&path).unwrap();
        let second = JsonlEventStore::open(&path);
        assert!(second.is_err());
    }

    #[test]
    fn load_filters_by_stream() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = store_in(&dir);
        let a = "lead/00000000-0000-7000-8000-00000000000a";
        let b = "lead/00000000-0000-7000-8000-00000000000b";
        store
            .append(
                a,
                0,
                &[pending(serde_json::json!({"s": "a"}))],
                Uuid::now_v7(),
            )
            .unwrap();
        store
            .append(
                b,
                0,
                &[pending(serde_json::json!({"s": "b"}))],
                Uuid::now_v7(),
            )
            .unwrap();
        assert_eq!(store.load(a).unwrap().len(), 1);
        assert_eq!(store.replay().unwrap().len(), 2);
    }
}
