//! The EventStore seam (design doc 0001 §1).

use jiff::Timestamp;
use miette::Result;
use uuid::Uuid;

use crate::domain::events::{ENVELOPE_VERSION, EventEnvelope, PendingEvent};

pub mod jsonl;
pub mod memory;
pub mod upcast;

pub use jsonl::JsonlEventStore;
pub(crate) use jsonl::read_only_envelopes;
pub(crate) use memory::MemStore;

/// Build the committed envelopes for a batch of pending events (the commit
/// unit, design doc §1): assign id, seq, timestamps, and causation chaining.
/// Shared by the JSONL store (which then serializes + fsyncs) and the
/// in-memory dry-run store (which accumulates them in memory).
pub(crate) fn build_batch(
    stream: &str,
    expected_seq: u64,
    events: &[PendingEvent],
    correlation_id: Uuid,
) -> Vec<EventEnvelope> {
    let now = Timestamp::now();
    let mut envelopes = Vec::with_capacity(events.len());
    for (i, pending) in events.iter().enumerate() {
        // Causation chaining within a batch: an event without an explicit
        // cause is caused by the event it follows (e.g. a `rejected` caused
        // by its `ingested`).
        let causation_id = pending
            .causation_id
            .or_else(|| envelopes.last().map(|e: &EventEnvelope| e.id));
        envelopes.push(EventEnvelope {
            envelope_version: ENVELOPE_VERSION,
            id: Uuid::now_v7(),
            stream: stream.to_string(),
            seq: expected_seq + 1 + i as u64,
            event_type: pending.event_type.to_string(),
            schema_version: pending.schema_version,
            occurred_at: pending.occurred_at.unwrap_or(now),
            recorded_at: now,
            causation_id,
            correlation_id,
            payload: pending.payload.clone(),
        });
    }
    envelopes
}

/// The only I/O seam between the domain and persistence.
///
/// Durability contract (design doc 0001 §1): a batch append is the commit
/// unit — all events in one `append` are written with a single `write(2)`
/// on an `O_APPEND` descriptor, followed by `fsync`. Replay discards a torn
/// trailing line (crash mid-write) with a warning; any other malformed line
/// is a hard error.
pub trait EventStore {
    /// Append a batch of events to `stream`. `expected_seq` is the caller's
    /// view of the stream's current sequence (optimistic concurrency);
    /// events are numbered `expected_seq + 1..`. All events in the batch
    /// share one `correlation_id`.
    fn append(
        &mut self,
        stream: &str,
        expected_seq: u64,
        events: &[PendingEvent],
        correlation_id: Uuid,
    ) -> Result<Vec<EventEnvelope>>;

    /// Load all events for one stream, in sequence order.
    fn load(&self, stream: &str) -> Result<Vec<EventEnvelope>>;

    /// Replay the entire log, in append order.
    fn replay(&self) -> Result<Vec<EventEnvelope>>;
}
