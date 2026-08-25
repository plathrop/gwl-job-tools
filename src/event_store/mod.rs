//! The EventStore seam (design doc 0001 §1).

use miette::Result;
use uuid::Uuid;

use crate::domain::events::{EventEnvelope, PendingEvent};

pub mod jsonl;
pub mod upcast;

pub use jsonl::JsonlEventStore;

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
