//! Append-only JSONL event store (design doc 0001 §1).
//!
//! - Single writer: an exclusive `flock` on `<log>.lock` is held for the
//!   store's lifetime; a second process fails fast.
//! - Batch append = commit unit: the whole batch is serialized and written
//!   with one `write_all` on an `O_APPEND` file, then `fsync`.
//! - Replay tolerates a torn trailing line (crash mid-write) with a warning;
//!   any other malformed line is a hard error.

use std::{
    fs::{File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

use fs2::FileExt;
use miette::{Context, IntoDiagnostic, Result, bail, miette};
use tracing::{instrument, warn};
use uuid::Uuid;

use crate::{
    domain::events::{ENVELOPE_VERSION, EventEnvelope, PendingEvent},
    event_store::{EventStore, build_batch, upcast::upcast},
};

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
        let bytes = std::fs::read(&self.path)
            .into_diagnostic()
            .wrap_err_with(|| format!("reading event log {}", self.path.display()))?;
        let (envelopes, torn) = parse_envelopes(&bytes, &self.path)?;
        if let Some(offset) = torn {
            // Commit policy (design doc §1): a torn tail was never committed.
            // The store holds the single-writer lock, so truncating it here
            // is safe and lets the next append start at a clean offset.
            warn!(
                "discarding and truncating uncommitted trailing record in event log \
                 (crash mid-write?)"
            );
            self.truncate_at(offset)?;
        }
        Ok(envelopes)
    }

    /// Truncate the log to `offset`, removing a torn tail. Safe because the
    /// store holds the single-writer lock for its lifetime. This is not
    /// "rewriting the log": the torn bytes were never a committed event —
    /// only complete, parseable events are sacred (design doc §1).
    fn truncate_at(&self, offset: usize) -> Result<()> {
        let file = OpenOptions::new()
            .write(true)
            .open(&self.path)
            .into_diagnostic()?;
        file.set_len(offset as u64).into_diagnostic()?;
        file.sync_all().into_diagnostic()?;
        Ok(())
    }
}

/// Parse envelopes from raw log bytes (design doc §1). `path` is for error
/// messages only. Returns the envelopes plus the byte offset of a torn
/// trailing line (`Some` when the final record is unterminated — never
/// committed). The caller decides whether to truncate: the store does
/// (it holds the lock); a read-only view does not.
fn parse_envelopes(bytes: &[u8], path: &Path) -> Result<(Vec<EventEnvelope>, Option<usize>)> {
    // Commit policy: every appended batch ends with '\n' and is fsync'd, so
    // a file that does not end with a newline has a torn tail — the final
    // record was never committed, whether or not its bytes happen to parse.
    // Only complete, newline-terminated, parseable events are sacred.
    let terminated = bytes.last() == Some(&b'\n');

    // Split on newlines at the byte level (never decoding the whole log up
    // front): a short write can split a multibyte UTF-8 character in
    // `raw_text`, and that must not turn into a hard UTF-8 error — the torn
    // final slice is recoverable, everything before it is not.
    let mut offset = 0usize;
    let mut lines: Vec<(usize, &[u8])> = Vec::new();
    for slice in bytes.split(|b| *b == b'\n') {
        lines.push((offset, slice));
        offset += slice.len() + 1;
    }

    let line_count = lines.len();
    let mut envelopes = Vec::new();
    for (idx, (line_offset, slice)) in lines.into_iter().enumerate() {
        if slice.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let is_last = idx == line_count - 1;
        if is_last && !terminated {
            return Ok((envelopes, Some(line_offset)));
        }
        // Syntax-level parse failure on a committed (newline-terminated)
        // line is corruption, as is any validation failure (envelope
        // version, upcast path) — hard errors wherever they appear.
        let mut envelope: EventEnvelope = serde_json::from_slice(slice)
            .into_diagnostic()
            .wrap_err_with(|| {
                format!(
                    "malformed event at {} line {} (refusing to replay a corrupt log)",
                    path.display(),
                    idx + 1
                )
            })?;
        envelope.payload = upcast(
            &envelope.event_type,
            envelope.schema_version,
            envelope.payload.take(),
        )
        .wrap_err_with(|| format!("event {} ({})", envelope.id, envelope.event_type))?;
        if envelope.envelope_version != ENVELOPE_VERSION {
            bail!(
                "unsupported envelope_version {} on event {} (this build understands \
                 {ENVELOPE_VERSION})",
                envelope.envelope_version,
                envelope.id
            );
        }
        envelopes.push(envelope);
    }
    Ok((envelopes, None))
}

/// Read the log WITHOUT the single-writer lock: a read-only view for preview
/// paths (`discover --dry-run`) and, later, read-only commands. A missing log
/// is an empty corpus (the store's `open` would create it; a read-only view
/// must not). A torn trailing line is discarded but NOT truncated — this
/// holder has no lock, so writing back would race a live writer.
pub(crate) fn read_only_envelopes(path: impl AsRef<Path>) -> Result<Vec<EventEnvelope>> {
    let path = path.as_ref();
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => {
            return Err(err)
                .into_diagnostic()
                .wrap_err_with(|| format!("reading event log {}", path.display()));
        }
    };
    let (envelopes, torn) = parse_envelopes(&bytes, path)?;
    if torn.is_some() {
        warn!(
            "ignoring uncommitted trailing record in read-only view of {} \
             (not truncating — no writer lock)",
            path.display()
        );
    }
    Ok(envelopes)
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

        let envelopes = build_batch(stream, expected_seq, events, correlation_id);
        let mut batch = String::new();
        for envelope in &envelopes {
            batch.push_str(&serde_json::to_string(envelope).into_diagnostic()?);
            batch.push('\n');
        }

        if !batch.is_empty() {
            // One write(2) for the whole batch (the commit unit), then
            // fsync. `write_all` can issue multiple write(2) calls after a
            // short write, so use a single `write` and treat a short count
            // as a failed/torn append (replay recovery will discard it).
            let mut file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)
                .into_diagnostic()?;
            let written = file.write(batch.as_bytes()).into_diagnostic()?;
            if written != batch.len() {
                bail!(
                    "short write appending to event log ({written} of {} bytes); \
                     the log may hold a torn tail that replay will discard",
                    batch.len()
                );
            }
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
            occurred_at: None,
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
    fn torn_trailing_line_is_discarded_and_truncated() {
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
        let good_len = std::fs::metadata(&path).unwrap().len();

        // Simulate a crash mid-write: partial JSON, no newline.
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"{\"envelope_version\":1,\"id\":").unwrap();
        drop(file);

        let replayed = store.replay().unwrap();
        assert_eq!(replayed.len(), 1);
        assert_eq!(replayed[0].payload["ok"], true);
        // The torn tail is truncated while the lock is held, so the next
        // append starts at a clean offset.
        assert_eq!(std::fs::metadata(&path).unwrap().len(), good_len);

        store
            .append(
                "lead/00000000-0000-7000-8000-000000000004",
                1,
                &[pending(serde_json::json!({"after": "torn"}))],
                Uuid::now_v7(),
            )
            .unwrap();
        assert_eq!(store.replay().unwrap().len(), 2);
    }

    #[test]
    fn unterminated_complete_record_is_torn_and_truncated() {
        // A short write can end exactly after a complete JSON object but
        // before its newline. The record parses, but its batch never
        // committed (every committed batch ends with '\n'), so it is torn:
        // discarded and truncated before the next append can corrupt it.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let mut store = JsonlEventStore::open(&path).unwrap();
        store
            .append(
                "lead/00000000-0000-7000-8000-000000000007",
                0,
                &[pending(serde_json::json!({"ok": true}))],
                Uuid::now_v7(),
            )
            .unwrap();
        let good_len = std::fs::metadata(&path).unwrap().len();

        // Complete, parseable JSON object, NO trailing newline.
        let orphan = serde_json::json!({
            "envelope_version": 1,
            "id": Uuid::now_v7(),
            "stream": "lead/00000000-0000-7000-8000-000000000007",
            "seq": 2,
            "type": "ingested",
            "schema_version": 1,
            "occurred_at": "2026-01-01T00:00:00Z",
            "recorded_at": "2026-01-01T00:00:00Z",
            "correlation_id": Uuid::now_v7(),
            "payload": {}
        });
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(serde_json::to_string(&orphan).unwrap().as_bytes())
            .unwrap();
        drop(file);

        let replayed = store.replay().unwrap();
        assert_eq!(replayed.len(), 1);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), good_len);
    }

    #[test]
    fn split_multibyte_char_in_torn_tail_is_recoverable() {
        // A short write can split a multibyte UTF-8 character in raw_text;
        // decoding the whole log up front would hard-fail on what is really
        // an ordinary torn tail.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let mut store = JsonlEventStore::open(&path).unwrap();
        store
            .append(
                "lead/00000000-0000-7000-8000-000000000008",
                0,
                &[pending(serde_json::json!({"ok": true}))],
                Uuid::now_v7(),
            )
            .unwrap();
        let good_len = std::fs::metadata(&path).unwrap().len();

        // First bytes of an incomplete record whose final character is a
        // split 4-byte UTF-8 sequence (e.g. an emoji in raw_text).
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"{\"envelope_version\":1,\"payload\":\"\xf0\x9f")
            .unwrap();
        drop(file);

        let replayed = store.replay().unwrap();
        assert_eq!(replayed.len(), 1);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), good_len);
    }

    #[test]
    fn complete_event_with_unknown_version_is_hard_error_even_on_last_line() {
        // A syntactically complete final line that fails validation is
        // corruption, not a torn tail — it must not be silently discarded.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let mut store = JsonlEventStore::open(&path).unwrap();
        store
            .append(
                "lead/00000000-0000-7000-8000-000000000006",
                0,
                &[pending(serde_json::json!({}))],
                Uuid::now_v7(),
            )
            .unwrap();

        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        let bogus = serde_json::json!({
            "envelope_version": 99,
            "id": Uuid::now_v7(),
            "stream": "lead/00000000-0000-7000-8000-000000000006",
            "seq": 2,
            "type": "ingested",
            "schema_version": 1,
            "occurred_at": "2026-01-01T00:00:00Z",
            "recorded_at": "2026-01-01T00:00:00Z",
            "correlation_id": Uuid::now_v7(),
            "payload": {}
        });
        writeln!(file, "{}", serde_json::to_string(&bogus).unwrap()).unwrap();
        drop(file);

        assert!(store.replay().is_err());
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
            .unwrap_err(); // append validates seq by loading, so it refuses too

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

    // ── read-only view (discover --dry-run) ──────────────────

    #[test]
    fn read_only_does_not_require_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let mut store = JsonlEventStore::open(&path).unwrap();
        store
            .append(
                "lead/00000000-0000-7000-8000-000000000009",
                0,
                &[pending(serde_json::json!({"ok": true}))],
                Uuid::now_v7(),
            )
            .unwrap();
        // The store above still holds the single-writer lock; the read-only
        // view must succeed anyway (it never takes the lock).
        let envelopes = read_only_envelopes(&path).unwrap();
        assert_eq!(envelopes.len(), 1);
        assert_eq!(envelopes[0].payload["ok"], true);
    }

    #[test]
    fn read_only_missing_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        assert!(read_only_envelopes(&path).unwrap().is_empty());
    }

    #[test]
    fn read_only_does_not_truncate_a_torn_tail() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let mut store = JsonlEventStore::open(&path).unwrap();
        store
            .append(
                "lead/00000000-0000-7000-8000-000000000010",
                0,
                &[pending(serde_json::json!({"ok": true}))],
                Uuid::now_v7(),
            )
            .unwrap();
        let good_len = std::fs::metadata(&path).unwrap().len();
        // Simulate a torn tail (partial record, no newline).
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"{\"envelope_version\":1,\"id\":").unwrap();
        drop(file);

        let envelopes = read_only_envelopes(&path).unwrap();
        assert_eq!(envelopes.len(), 1);
        // Read-only view must NOT truncate the torn tail (no lock held).
        assert!(
            std::fs::metadata(&path).unwrap().len() > good_len,
            "read-only view must not truncate the torn tail"
        );
    }
}
