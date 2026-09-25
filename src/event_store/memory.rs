//! In-memory event store for dry-run previews: the same decide → append →
//! replay semantics as the JSONL store, against throwaway storage.

use std::collections::HashMap;

use miette::{Result, bail};
use uuid::Uuid;

use crate::{
    domain::events::{EventEnvelope, PendingEvent},
    event_store::{EventStore, build_batch},
};

/// An event store that accumulates envelopes in memory. Seed it with
/// [`MemStore::seeded`] (the read-only replay of the real corpus) so
/// identity lookup sees prior leads; appends are discarded on drop.
#[derive(Default)]
pub(crate) struct MemStore {
    events: Vec<EventEnvelope>,
    by_stream: HashMap<String, Vec<EventEnvelope>>,
}

impl MemStore {
    /// A store seeded with an existing corpus, preserving envelope order
    /// (so replay → projection is identical to the real log's).
    pub(crate) fn seeded(events: Vec<EventEnvelope>) -> Self {
        let mut store = Self::default();
        for event in events {
            store
                .by_stream
                .entry(event.stream.clone())
                .or_default()
                .push(event.clone());
            store.events.push(event);
        }
        store
    }
}

impl EventStore for MemStore {
    fn append(
        &mut self,
        stream: &str,
        expected_seq: u64,
        events: &[PendingEvent],
        correlation_id: Uuid,
    ) -> Result<Vec<EventEnvelope>> {
        // Optimistic concurrency, mirroring the JSONL store: the caller's
        // view must be current. (In-memory there is no cross-process race,
        // but the decide → append discipline is the same.)
        let current_seq = self
            .by_stream
            .get(stream)
            .and_then(|v| v.last())
            .map(|e| e.seq)
            .unwrap_or(0);
        if current_seq != expected_seq {
            bail!(
                "concurrent modification of stream '{stream}': expected seq \
                 {expected_seq}, found {current_seq}"
            );
        }
        let envelopes = build_batch(stream, expected_seq, events, correlation_id);
        self.by_stream
            .entry(stream.to_string())
            .or_default()
            .extend(envelopes.iter().cloned());
        self.events.extend(envelopes.iter().cloned());
        Ok(envelopes)
    }

    fn load(&self, stream: &str) -> Result<Vec<EventEnvelope>> {
        Ok(self.by_stream.get(stream).cloned().unwrap_or_default())
    }

    fn replay(&self) -> Result<Vec<EventEnvelope>> {
        Ok(self.events.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::events::event_type;

    fn pending(n: u64) -> PendingEvent {
        PendingEvent::new(event_type::SCORED, None, &serde_json::json!({"n": n})).unwrap()
    }

    // ── append / load / replay round-trip ────────────────────────

    #[test]
    fn append_then_replay_roundtrip() {
        let mut store = MemStore::default();
        let correlation = Uuid::now_v7();
        let written = store
            .append("lead/a", 0, &[pending(1)], correlation)
            .unwrap();
        assert_eq!(written[0].seq, 1);
        assert_eq!(written[0].correlation_id, correlation);
        assert_eq!(store.replay().unwrap().len(), 1);
    }

    #[test]
    fn batch_append_numbers_seqs_consecutively() {
        let mut store = MemStore::default();
        store
            .append("lead/b", 0, &[pending(1), pending(2)], Uuid::now_v7())
            .unwrap();
        let seqs: Vec<u64> = store
            .load("lead/b")
            .unwrap()
            .iter()
            .map(|e| e.seq)
            .collect();
        assert_eq!(seqs, vec![1, 2]);
    }

    #[test]
    fn stale_expected_seq_bails() {
        let mut store = MemStore::default();
        store
            .append("lead/c", 0, &[pending(1)], Uuid::now_v7())
            .unwrap();
        assert!(
            store
                .append("lead/c", 0, &[pending(2)], Uuid::now_v7())
                .is_err()
        );
    }

    #[test]
    fn seeded_store_replays_seed_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let mut real =
            crate::event_store::JsonlEventStore::open(dir.path().join("events.jsonl")).unwrap();
        real.append("lead/d", 0, &[pending(1)], Uuid::now_v7())
            .unwrap();
        real.append("lead/d", 1, &[pending(2)], Uuid::now_v7())
            .unwrap();
        let seed = real.replay().unwrap();

        let mem = MemStore::seeded(seed.clone());
        let replayed = mem.replay().unwrap();
        assert_eq!(replayed.len(), seed.len());
        assert_eq!(replayed[0].seq, 1);
        assert_eq!(replayed[1].seq, 2);
        // A subsequent append continues the seeded stream's sequence.
        let mut mem = mem;
        mem.append("lead/d", 2, &[pending(3)], Uuid::now_v7())
            .unwrap();
        assert_eq!(mem.load("lead/d").unwrap().len(), 3);
    }
}
