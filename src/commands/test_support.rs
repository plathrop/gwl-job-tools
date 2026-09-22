//! Shared test helpers for the per-command test modules (cfg(test)).
//!
//! Kept in one place so the command tests don't each re-derive the same
//! fixtures (a temp-dir store, a synthesized `IngestOutcome`, a synthetic
//! `LeadRecord`, …).

use jiff::Timestamp;
use uuid::Uuid;

use crate::{
    commands::record_ingest,
    config::Config,
    domain::events::ExtractedFields,
    event_store::{EventStore, JsonlEventStore},
    ingest::IngestOutcome,
    projections::{self, LeadRecord, Projection},
};

pub(crate) fn outcome(raw_text: &str) -> IngestOutcome {
    IngestOutcome {
        adapter: "drop-in".into(),
        source: "unknown".into(),
        url: None,
        raw_text: raw_text.into(),
        extracted: ExtractedFields::default(),
    }
}

pub(crate) fn store_and_projection(dir: &tempfile::TempDir) -> (JsonlEventStore, Projection) {
    let store = JsonlEventStore::open(dir.path().join("events.jsonl")).unwrap();
    let projection = projections::rebuild(&store.replay().unwrap()).unwrap();
    (store, projection)
}

pub(crate) fn projection_with_leads(dir: &tempfile::TempDir, count: usize) -> Projection {
    let mut store = JsonlEventStore::open(dir.path().join("events.jsonl")).unwrap();
    let mut projection = projections::rebuild(&[]).unwrap();
    for _ in 0..count {
        let mut outcome = outcome("body");
        outcome.url = Some(format!("https://example.com/{}", Uuid::now_v7()));
        record_ingest(&mut store, &projection, &Config::default(), &[], outcome).unwrap();
        projection = projections::rebuild(&store.replay().unwrap()).unwrap();
    }
    projection
}

/// Ingest a lead and return (store, its record) with a fresh projection.
pub(crate) fn ingested_lead(
    dir: &tempfile::TempDir,
    config: &Config,
    extracted: ExtractedFields,
    raw_text: &str,
) -> (JsonlEventStore, LeadRecord) {
    let mut store = JsonlEventStore::open(dir.path().join("events.jsonl")).unwrap();
    let projection = projections::rebuild(&store.replay().unwrap()).unwrap();
    let mut o = outcome(raw_text);
    o.url = Some(format!("https://example.com/{}", Uuid::now_v7()));
    o.extracted = extracted;
    let summary = record_ingest(&mut store, &projection, config, &[], o).unwrap();
    let projection = projections::rebuild(&store.replay().unwrap()).unwrap();
    let record = projection.leads.get(&summary.lead_id).unwrap().clone();
    (store, record)
}

pub(crate) fn lead_record(url: Option<&str>) -> LeadRecord {
    LeadRecord {
        lead_id: Uuid::now_v7(),
        dedupe_key: None,
        identifiers: crate::domain::events::Identifiers::default(),
        adapter: None,
        source: None,
        url: url.map(Into::into),
        extracted: ExtractedFields::default(),
        latest_mark: None,
        deferral_count: 0,
        apply_queued: false,
        latest_rejection: None,
        latest_score: None,
        latest_outcome: None,
        event_count: 0,
        first_seen: Timestamp::now(),
        last_event: Timestamp::now(),
    }
}
