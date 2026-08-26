//! The Lead aggregate: pure `decide` / `evolve` over replayed state.
//!
//! Design doc 0001 §2 (identity & re-ingest semantics) and §1 (aggregate
//! pattern). Gates and scoring land in Increments 2–3; the aggregate already
//! understands `reviewed` marks so the durable-ignore suppression rule is in
//! place before marks exist.

use miette::Result;
use serde_json::Value;
use uuid::Uuid;

use crate::domain::{
    events::{
        EventEnvelope, ExtractedFields, IngestedPayload, PendingEvent, ReingestSuppressedPayload,
        SnapshotFields, UpdatedPayload, event_type,
    },
    identity::LeadIdentity,
};

/// Parse the lead id out of a `lead/<uuid>` stream name. Lives here (not on
/// the stream-agnostic `EventEnvelope`) because the `lead/` prefix is lead
/// aggregate knowledge.
pub fn stream_lead_id(stream: &str) -> Option<Uuid> {
    stream
        .strip_prefix("lead/")
        .and_then(|s| Uuid::parse_str(s).ok())
}

/// Replayed state of a single lead stream.
#[derive(Clone, Debug, Default)]
pub struct LeadState {
    pub seq: u64,
    pub exists: bool,
    pub latest_mark: Option<String>,
    pub snapshot: Option<ExtractedFields>,
    pub url: Option<String>,
    pub raw_text: Option<String>,
}

impl LeadState {
    pub fn stream_id(lead_id: Uuid) -> String {
        format!("lead/{lead_id}")
    }
}

pub fn evolve(state: &mut LeadState, event: &EventEnvelope) {
    state.seq = event.seq;
    match event.event_type.as_str() {
        event_type::INGESTED | event_type::UPDATED => {
            state.exists = true;
            if let Ok(snapshot) = serde_json::from_value::<SnapshotFields>(event.payload.clone()) {
                state.snapshot = Some(snapshot.extracted);
                state.url = snapshot.url;
                state.raw_text = snapshot.raw_text;
            }
        }
        // Marks are latest-wins (design doc §3). The `reviewed` event lands
        // with Increment 4; the suppression rule below already honors it.
        event_type::REVIEWED => {
            state.latest_mark = event
                .payload
                .get("mark")
                .and_then(Value::as_str)
                .map(str::to_string);
        }
        _ => {}
    }
}

/// What an ingest command decided to append.
#[derive(Clone, Debug, PartialEq)]
pub enum IngestKind {
    New,
    Updated { changed: Vec<String> },
    Suppressed,
}

/// Decide the events for an ingest, given the replayed state of the matched
/// stream (default state = no match, i.e. a new lead).
///
/// - No match → `ingested`.
/// - Match, latest mark `ignore` → `reingest_suppressed` (durable; design
///   doc §2). No re-gating, no re-scoring, no queue re-entry.
/// - Match, otherwise → `updated` with the new snapshot and changed-field
///   list.
pub fn decide_ingest(
    state: &LeadState,
    identity: &LeadIdentity,
    source: &str,
    url: Option<String>,
    raw_text: String,
    extracted: ExtractedFields,
) -> Result<(IngestKind, Vec<PendingEvent>)> {
    if !state.exists {
        let payload = IngestedPayload {
            dedupe_key: identity.dedupe_key.clone(),
            identifiers: identity.identifiers.clone(),
            snapshot: SnapshotFields {
                source: source.into(),
                url,
                raw_text: Some(raw_text),
                extracted,
            },
        };
        return Ok((
            IngestKind::New,
            vec![PendingEvent::new(event_type::INGESTED, None, &payload)?],
        ));
    }

    if state.latest_mark.as_deref() == Some("ignore") {
        let payload = ReingestSuppressedPayload {
            dedupe_key: identity.dedupe_key.clone(),
            suppressed_by_mark: "ignore".into(),
            identifiers: identity.identifiers.clone(),
        };
        return Ok((
            IngestKind::Suppressed,
            vec![PendingEvent::new(
                event_type::REINGEST_SUPPRESSED,
                None,
                &payload,
            )?],
        ));
    }

    let old = state.snapshot.clone().unwrap_or_default();
    let changed = old.diff(
        &extracted,
        state.raw_text.as_deref() != Some(raw_text.as_str()),
        state.url != url,
    );
    let payload = UpdatedPayload {
        dedupe_key: identity.dedupe_key.clone(),
        identifiers: identity.identifiers.clone(),
        changed: changed.clone(),
        snapshot: SnapshotFields {
            source: source.into(),
            url,
            raw_text: Some(raw_text),
            extracted,
        },
    };
    Ok((
        IngestKind::Updated { changed },
        vec![PendingEvent::new(event_type::UPDATED, None, &payload)?],
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::events::{Identifiers, event_type};

    fn identity() -> LeadIdentity {
        LeadIdentity {
            dedupe_key: "url:https://example.com/j".into(),
            identifiers: Identifiers {
                req: None,
                url: Some("url:https://example.com/j".into()),
                tc: None,
            },
        }
    }

    fn extracted(title: &str) -> ExtractedFields {
        ExtractedFields {
            title: Some(title.into()),
            company: Some("Acme".into()),
            ..Default::default()
        }
    }

    // ── decide_ingest ────────────────────────────────────────────

    #[test]
    fn new_lead_emits_ingested() {
        let (kind, events) = decide_ingest(
            &LeadState::default(),
            &identity(),
            "drop-in",
            Some("https://example.com/j".into()),
            "body".into(),
            extracted("Engineer"),
        )
        .unwrap();
        assert_eq!(kind, IngestKind::New);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, event_type::INGESTED);
        assert_eq!(events[0].payload["dedupe_key"], "url:https://example.com/j");
    }

    #[test]
    fn existing_lead_emits_updated_with_changed_fields() {
        let mut state = LeadState {
            exists: true,
            seq: 1,
            ..Default::default()
        };
        state.snapshot = Some(extracted("Engineer"));
        state.url = Some("https://example.com/j".into());
        state.raw_text = Some("body".into());

        let (kind, events) = decide_ingest(
            &state,
            &identity(),
            "drop-in",
            Some("https://example.com/j".into()),
            "body".into(),
            extracted("Senior Engineer"),
        )
        .unwrap();
        assert_eq!(
            kind,
            IngestKind::Updated {
                changed: vec!["title".into()]
            }
        );
        assert_eq!(events[0].event_type, event_type::UPDATED);
        assert_eq!(events[0].payload["changed"][0], "title");
    }

    #[test]
    fn ignored_lead_emits_suppressed() {
        let state = LeadState {
            exists: true,
            latest_mark: Some("ignore".into()),
            ..Default::default()
        };
        let (kind, events) = decide_ingest(
            &state,
            &identity(),
            "drop-in",
            Some("https://example.com/j".into()),
            "body".into(),
            extracted("Engineer"),
        )
        .unwrap();
        assert_eq!(kind, IngestKind::Suppressed);
        assert_eq!(events[0].event_type, event_type::REINGEST_SUPPRESSED);
        assert_eq!(events[0].payload["suppressed_by_mark"], "ignore");
    }

    #[test]
    fn non_ignore_mark_does_not_suppress() {
        let mut state = LeadState {
            exists: true,
            latest_mark: Some("defer".into()),
            ..Default::default()
        };
        state.snapshot = Some(extracted("Engineer"));
        state.raw_text = Some("body".into());
        let (kind, _) = decide_ingest(
            &state,
            &identity(),
            "drop-in",
            Some("https://example.com/j".into()),
            "body".into(),
            extracted("Engineer"),
        )
        .unwrap();
        assert!(matches!(kind, IngestKind::Updated { .. }));
    }

    #[test]
    fn updated_reports_url_change() {
        let mut state = LeadState {
            exists: true,
            seq: 1,
            ..Default::default()
        };
        state.snapshot = Some(extracted("Engineer"));
        state.url = Some("https://example.com/old".into());
        state.raw_text = Some("body".into());

        let (kind, events) = decide_ingest(
            &state,
            &identity(),
            "drop-in",
            Some("https://example.com/new".into()),
            "body".into(),
            extracted("Engineer"),
        )
        .unwrap();
        assert_eq!(
            kind,
            IngestKind::Updated {
                changed: vec!["url".into()]
            }
        );
        assert_eq!(events[0].payload["changed"][0], "url");
    }

    // ── evolve ───────────────────────────────────────────────────

    #[test]
    fn evolve_tracks_snapshot_and_mark() {
        let mut state = LeadState::default();
        let ingested = EventEnvelope {
            envelope_version: 1,
            id: Uuid::now_v7(),
            stream: "lead/x".into(),
            seq: 1,
            event_type: event_type::INGESTED.into(),
            schema_version: 1,
            occurred_at: jiff::Timestamp::now(),
            recorded_at: jiff::Timestamp::now(),
            causation_id: None,
            correlation_id: Uuid::now_v7(),
            payload: serde_json::json!({
                "dedupe_key": "tc:abc",
                "identifiers": {"tc": "tc:abc"},
                "source": "drop-in",
                "raw_text": "body",
                "extracted": {"title": "Engineer", "company": "Acme"}
            }),
        };
        evolve(&mut state, &ingested);
        assert!(state.exists);
        assert_eq!(state.seq, 1);
        assert_eq!(
            state.snapshot.as_ref().unwrap().title.as_deref(),
            Some("Engineer")
        );

        let reviewed = EventEnvelope {
            event_type: event_type::REVIEWED.into(),
            seq: 2,
            payload: serde_json::json!({"mark": "ignore"}),
            ..ingested
        };
        evolve(&mut state, &reviewed);
        assert_eq!(state.latest_mark.as_deref(), Some("ignore"));
        assert_eq!(state.seq, 2);
    }

    #[test]
    fn evolve_ignores_unknown_event_types() {
        let mut state = LeadState::default();
        let unknown = EventEnvelope {
            envelope_version: 1,
            id: Uuid::now_v7(),
            stream: "lead/x".into(),
            seq: 7,
            event_type: "some_future_event".into(),
            schema_version: 1,
            occurred_at: jiff::Timestamp::now(),
            recorded_at: jiff::Timestamp::now(),
            causation_id: None,
            correlation_id: Uuid::now_v7(),
            payload: serde_json::json!({}),
        };
        evolve(&mut state, &unknown);
        assert_eq!(state.seq, 7);
        assert!(!state.exists);
    }

    #[test]
    fn replay_then_decide_suppresses_ignored_lead() {
        // Integration: replay an ingested → reviewed{ignore} stream through
        // evolve, then decide_ingest must suppress (not update). The unit
        // test above constructs the state by hand; this exercises the real
        // replay path the command layer uses.
        let mut state = LeadState::default();
        let ingested = EventEnvelope {
            envelope_version: 1,
            id: Uuid::now_v7(),
            stream: "lead/x".into(),
            seq: 1,
            event_type: event_type::INGESTED.into(),
            schema_version: 1,
            occurred_at: jiff::Timestamp::now(),
            recorded_at: jiff::Timestamp::now(),
            causation_id: None,
            correlation_id: Uuid::now_v7(),
            payload: serde_json::json!({
                "dedupe_key": "tc:abc",
                "identifiers": {"tc": "tc:abc"},
                "source": "drop-in",
                "raw_text": "body",
                "extracted": {"title": "Engineer", "company": "Acme"}
            }),
        };
        evolve(&mut state, &ingested);
        let reviewed = EventEnvelope {
            event_type: event_type::REVIEWED.into(),
            seq: 2,
            payload: serde_json::json!({"mark": "ignore"}),
            ..ingested
        };
        evolve(&mut state, &reviewed);

        let (kind, events) = decide_ingest(
            &state,
            &identity(),
            "drop-in",
            Some("https://example.com/j".into()),
            "body".into(),
            extracted("Engineer"),
        )
        .unwrap();
        assert_eq!(kind, IngestKind::Suppressed);
        assert_eq!(events[0].event_type, event_type::REINGEST_SUPPRESSED);
    }
}
