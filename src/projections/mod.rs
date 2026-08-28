//! In-memory projections, rebuilt by replaying the log at startup (design
//! doc 0001 §7): the LeadIndex (identifier forms → lead_id, drives §2
//! matching) and the LeadBook (per-lead latest snapshot).

use std::collections::HashMap;

use jiff::Timestamp;
use miette::{IntoDiagnostic, Result};
use serde::Serialize;
use serde_json::Value;
use serde_with::skip_serializing_none;
use uuid::Uuid;

use crate::domain::{
    events::{
        EventEnvelope, ExtractedFields, Identifiers, OutcomePayload, ScoredPayload, event_type,
    },
    identity::LeadIdentity,
    lead::stream_lead_id,
};

#[skip_serializing_none]
#[derive(Clone, Debug, Serialize)]
pub struct LeadRecord {
    pub lead_id: Uuid,
    pub dedupe_key: Option<String>,
    pub identifiers: Identifiers,
    pub source: Option<String>,
    pub url: Option<String>,
    pub extracted: ExtractedFields,
    pub latest_mark: Option<String>,
    /// The most recent gate failure, if the last gate evaluation rejected
    /// the lead. Cleared by any subsequent `ingested`/`updated` that passed
    /// gates (a `rejected` follows those in the same batch when it fails).
    pub latest_rejection: Option<GateRejection>,
    /// The latest `scored` payload, if the lead has ever passed gates.
    pub latest_score: Option<ScoredPayload>,
    /// The latest user-recorded outcome (applied/screened/…/accepted/…), if
    /// any. Drives the review queue's "no outcome event" membership rule.
    pub latest_outcome: Option<OutcomeView>,
    pub event_count: u64,
    pub first_seen: Timestamp,
    pub last_event: Timestamp,
}

#[derive(Clone, Debug, Serialize)]
pub struct GateRejection {
    pub gate: String,
    pub reason: String,
    pub revision: u64,
}

/// A user-recorded outcome event's projected state (design doc 0001 §3).
#[derive(Clone, Debug, Serialize)]
pub struct OutcomeView {
    pub event_type: String,
    pub note: Option<String>,
    pub occurred_at: Timestamp,
}

#[derive(Clone, Debug, Default)]
pub struct Projection {
    pub leads: HashMap<Uuid, LeadRecord>,
    req_index: HashMap<String, Uuid>,
    url_index: HashMap<String, Uuid>,
    tc_index: HashMap<String, Uuid>,
    /// Every lead's dedupe key, indexed. This makes "the dedupe key is
    /// always matchable" an invariant — it is what covers the `raw:`
    /// fallback form, which has no slot in `Identifiers`.
    dedupe_index: HashMap<String, Uuid>,
}

impl Projection {
    /// Match an incoming posting's identity against the index (design doc
    /// §2): `req:` and `url:` hits always match, checked in precedence
    /// order; the `tc:` fallback is consulted **only** when the incoming
    /// posting carries neither a req nor a URL identifier; finally, the
    /// dedupe key itself matches (covers `raw:`-keyed leads).
    pub fn lookup(&self, identity: &LeadIdentity) -> Option<Uuid> {
        if let Some(req) = &identity.identifiers.req
            && let Some(id) = self.req_index.get(req)
        {
            return Some(*id);
        }
        if let Some(url) = &identity.identifiers.url
            && let Some(id) = self.url_index.get(url)
        {
            return Some(*id);
        }
        if identity.identifiers.req.is_none()
            && identity.identifiers.url.is_none()
            && let Some(tc) = &identity.identifiers.tc
            && let Some(id) = self.tc_index.get(tc)
        {
            return Some(*id);
        }
        self.dedupe_index.get(&identity.dedupe_key).copied()
    }

    /// Leads whose id (hyphenated) starts with the given prefix. Used for
    /// `<lead>` addressing by unambiguous UUID prefix (design doc §8).
    pub fn find_by_id_prefix(&self, prefix: &str) -> Vec<&LeadRecord> {
        let mut matches: Vec<&LeadRecord> = self
            .leads
            .values()
            .filter(|r| r.lead_id.to_string().starts_with(prefix))
            .collect();
        matches.sort_by_key(|r| r.first_seen);
        matches
    }
}

#[derive(serde::Deserialize)]
struct IngestView {
    dedupe_key: String,
    #[serde(default)]
    identifiers: Identifiers,
    source: String,
    url: Option<String>,
    #[serde(default)]
    extracted: ExtractedFields,
}

pub fn rebuild(events: &[EventEnvelope]) -> Result<Projection> {
    let mut projection = Projection::default();
    for event in events {
        let Some(lead_id) = stream_lead_id(&event.stream) else {
            continue;
        };
        match event.event_type.as_str() {
            event_type::INGESTED | event_type::UPDATED => {
                // A payload we cannot decode is source-of-truth corruption,
                // not something to skip: a stale projection can mint a
                // duplicate lead on the next ingest. Fail loudly with the
                // event's identity.
                let view = serde_json::from_value::<IngestView>(event.payload.clone())
                    .into_diagnostic()
                    .map_err(|e| {
                        e.wrap_err(format!(
                            "decoding {} payload of event {} (seq {})",
                            event.event_type, event.id, event.seq
                        ))
                    })?;
                projection
                    .dedupe_index
                    .insert(view.dedupe_key.clone(), lead_id);
                index_identifiers(&mut projection, lead_id, &view.identifiers);
                let record = projection
                    .leads
                    .entry(lead_id)
                    .or_insert_with(|| LeadRecord {
                        lead_id,
                        dedupe_key: None,
                        identifiers: Identifiers::default(),
                        source: None,
                        url: None,
                        extracted: ExtractedFields::default(),
                        latest_mark: None,
                        latest_rejection: None,
                        latest_score: None,
                        latest_outcome: None,
                        event_count: 0,
                        first_seen: event.recorded_at,
                        last_event: event.recorded_at,
                    });
                record.dedupe_key = Some(view.dedupe_key);
                record.identifiers = view.identifiers;
                record.source = Some(view.source);
                // Mirror the event snapshot exactly: an `updated` that
                // drops the URL (e.g. a file re-ingest of a URL-ingested
                // lead) removes it here too.
                record.url = view.url;
                record.extracted = view.extracted;
                record.latest_rejection = None;
                // A new snapshot invalidates the previous score (decision
                // 0006): the new evaluation's `scored` event follows in the
                // same batch, and a torn batch must not present a stale score.
                record.latest_score = None;
                record.event_count += 1;
                record.last_event = event.recorded_at;
            }
            event_type::REVIEWED => {
                if let Some(record) = projection.leads.get_mut(&lead_id) {
                    record.latest_mark = event
                        .payload
                        .get("mark")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    record.event_count += 1;
                    record.last_event = event.recorded_at;
                }
            }
            event_type::REJECTED => {
                // Strict, like the ingest payloads: a mistyped rejection is
                // source-of-truth corruption, not a None.
                let payload: crate::domain::events::RejectedPayload =
                    serde_json::from_value(event.payload.clone())
                        .into_diagnostic()
                        .map_err(|e| {
                            e.wrap_err(format!(
                                "decoding rejected payload of event {} (seq {})",
                                event.id, event.seq
                            ))
                        })?;
                if let Some(record) = projection.leads.get_mut(&lead_id) {
                    record.latest_rejection = Some(GateRejection {
                        gate: payload.gate,
                        reason: payload.reason,
                        revision: payload.revision,
                    });
                    record.event_count += 1;
                    record.last_event = event.recorded_at;
                }
            }
            event_type::REINGEST_SUPPRESSED => {
                // A suppressed repost still teaches the index its
                // identifiers: a later copy carrying only one of these
                // forms must keep matching the durably ignored lead.
                #[derive(serde::Deserialize)]
                struct SuppressedView {
                    dedupe_key: String,
                    #[serde(default)]
                    identifiers: Identifiers,
                }
                if let Ok(view) = serde_json::from_value::<SuppressedView>(event.payload.clone()) {
                    projection.dedupe_index.insert(view.dedupe_key, lead_id);
                    index_identifiers(&mut projection, lead_id, &view.identifiers);
                }
                if let Some(record) = projection.leads.get_mut(&lead_id) {
                    record.event_count += 1;
                    record.last_event = event.recorded_at;
                }
            }
            event_type::SCORED => {
                // Strict, like the ingest payloads: a mistyped score is
                // source-of-truth corruption, not a None.
                let payload: ScoredPayload = serde_json::from_value(event.payload.clone())
                    .into_diagnostic()
                    .map_err(|e| {
                        e.wrap_err(format!(
                            "decoding scored payload of event {} (seq {})",
                            event.id, event.seq
                        ))
                    })?;
                if let Some(record) = projection.leads.get_mut(&lead_id) {
                    record.latest_score = Some(payload);
                    record.event_count += 1;
                    record.last_event = event.recorded_at;
                }
            }
            event_type::APPLIED
            | event_type::SCREENED
            | event_type::INTERVIEWED
            | event_type::OFFERED
            | event_type::ACCEPTED
            | event_type::REJECTED_BY_EMPLOYER
            | event_type::WITHDRAWN
            | event_type::DECLINED
            | event_type::UNRESPONSIVE
            | event_type::ARCHIVED => {
                let payload: OutcomePayload = serde_json::from_value(event.payload.clone())
                    .into_diagnostic()
                    .map_err(|e| {
                        e.wrap_err(format!(
                            "decoding outcome payload of event {} (seq {})",
                            event.id, event.seq
                        ))
                    })?;
                if let Some(record) = projection.leads.get_mut(&lead_id) {
                    record.latest_outcome = Some(OutcomeView {
                        event_type: event.event_type.clone(),
                        note: payload.note,
                        occurred_at: event.occurred_at,
                    });
                    record.event_count += 1;
                    record.last_event = event.recorded_at;
                }
            }
            _ => {
                if let Some(record) = projection.leads.get_mut(&lead_id) {
                    record.event_count += 1;
                    record.last_event = event.recorded_at;
                }
            }
        }
    }
    Ok(projection)
}

fn index_identifiers(projection: &mut Projection, lead_id: Uuid, identifiers: &Identifiers) {
    if let Some(req) = &identifiers.req {
        projection.req_index.insert(req.clone(), lead_id);
    }
    if let Some(url) = &identifiers.url {
        projection.url_index.insert(url.clone(), lead_id);
    }
    if let Some(tc) = &identifiers.tc {
        projection.tc_index.insert(tc.clone(), lead_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::events::ENVELOPE_VERSION;

    fn envelope(
        lead_id: Uuid,
        seq: u64,
        event_type: &str,
        payload: serde_json::Value,
    ) -> EventEnvelope {
        EventEnvelope {
            envelope_version: ENVELOPE_VERSION,
            id: Uuid::now_v7(),
            stream: format!("lead/{lead_id}"),
            seq,
            event_type: event_type.into(),
            schema_version: 1,
            occurred_at: Timestamp::now(),
            recorded_at: Timestamp::now(),
            causation_id: None,
            correlation_id: Uuid::now_v7(),
            payload,
        }
    }

    fn ingested_payload(req: Option<&str>, url: Option<&str>, tc: Option<&str>) -> Value {
        let identifiers = Identifiers {
            req: req.map(Into::into),
            url: url.map(Into::into),
            tc: tc.map(Into::into),
        };
        serde_json::json!({
            "dedupe_key": req.or(url).or(tc).unwrap(),
            "identifiers": identifiers,
            "source": "drop-in",
            "raw_text": "body",
            "extracted": {"title": "Engineer", "company": "Acme"}
        })
    }

    fn scored_payload(composite: u64, revision: u64) -> Value {
        serde_json::json!({
            "composite": composite,
            "revision": revision,
            "dimensions": [
                {"name": "level", "score": 100, "weight": 1.0, "confidence": 1.0},
                {"name": "remote", "score": 50, "weight": 1.0, "confidence": 1.0}
            ],
            "breakdown": format!("{composite} = 0.5·level(100) + 0.5·remote(50)")
        })
    }

    // ── rebuild ──────────────────────────────────────────────────

    #[test]
    fn rebuild_indexes_all_identifier_forms() {
        let lead_id = Uuid::now_v7();
        let events = vec![envelope(
            lead_id,
            1,
            event_type::INGESTED,
            ingested_payload(
                Some("req:acme:r-1"),
                Some("url:https://example.com/j"),
                Some("tc:abc"),
            ),
        )];
        let projection = rebuild(&events).unwrap();
        let record = projection.leads.get(&lead_id).unwrap();
        assert_eq!(record.extracted.title.as_deref(), Some("Engineer"));
        assert_eq!(record.event_count, 1);

        for form in ["req:acme:r-1", "url:https://example.com/j", "tc:abc"] {
            let owned = form.to_string();
            let identity = LeadIdentity {
                dedupe_key: owned.clone(),
                identifiers: Identifiers {
                    req: form.starts_with("req:").then(|| owned.clone()),
                    url: form.starts_with("url:").then(|| owned.clone()),
                    tc: form.starts_with("tc:").then(|| owned.clone()),
                },
            };
            assert_eq!(projection.lookup(&identity), Some(lead_id), "form {form}");
        }
    }

    #[test]
    fn updated_refreshes_snapshot_and_counts() {
        let lead_id = Uuid::now_v7();
        let events = vec![
            envelope(
                lead_id,
                1,
                event_type::INGESTED,
                ingested_payload(None, Some("url:https://example.com/j"), None),
            ),
            envelope(
                lead_id,
                2,
                event_type::UPDATED,
                serde_json::json!({
                    "dedupe_key": "url:https://example.com/j",
                    "identifiers": {"url": "url:https://example.com/j"},
                    "changed": ["title"],
                    "source": "drop-in",
                    "raw_text": "body2",
                    "extracted": {"title": "Senior Engineer", "company": "Acme"}
                }),
            ),
        ];
        let projection = rebuild(&events).unwrap();
        let record = projection.leads.get(&lead_id).unwrap();
        assert_eq!(record.extracted.title.as_deref(), Some("Senior Engineer"));
        assert_eq!(record.event_count, 2);
    }

    // ── scored projection (decision 0006) ─────────────────────────

    #[test]
    fn snapshot_invalidates_stale_score() {
        // `scored` is the pass-marker: a new snapshot whose scored event was
        // torn off by a crash mid-batch must not present the lead as passing
        // on an old score. Mirrors the latest_rejection clearing above.
        let lead_id = Uuid::now_v7();
        let events = vec![
            envelope(
                lead_id,
                1,
                event_type::INGESTED,
                ingested_payload(None, Some("url:https://example.com/j"), None),
            ),
            envelope(lead_id, 2, event_type::SCORED, scored_payload(75, 1)),
            envelope(
                lead_id,
                3,
                event_type::UPDATED,
                serde_json::json!({
                    "dedupe_key": "url:https://example.com/j",
                    "identifiers": {"url": "url:https://example.com/j"},
                    "changed": ["title"],
                    "source": "drop-in",
                    "raw_text": "body2",
                    "extracted": {"title": "Senior Engineer", "company": "Acme"}
                }),
            ),
        ];
        let projection = rebuild(&events).unwrap();
        let record = projection.leads.get(&lead_id).unwrap();
        assert!(record.latest_score.is_none());
    }

    #[test]
    fn snapshot_then_scored_sets_current_score() {
        // Intact batch: the scored event after the snapshot restores the
        // marker; on re-evaluation the latest revision wins.
        let lead_id = Uuid::now_v7();
        let updated = serde_json::json!({
            "dedupe_key": "url:https://example.com/j",
            "identifiers": {"url": "url:https://example.com/j"},
            "changed": ["title"],
            "source": "drop-in",
            "raw_text": "body2",
            "extracted": {"title": "Senior Engineer", "company": "Acme"}
        });
        let events = vec![
            envelope(
                lead_id,
                1,
                event_type::INGESTED,
                ingested_payload(None, Some("url:https://example.com/j"), None),
            ),
            envelope(lead_id, 2, event_type::SCORED, scored_payload(75, 1)),
            envelope(lead_id, 3, event_type::UPDATED, updated),
            envelope(lead_id, 4, event_type::SCORED, scored_payload(80, 2)),
        ];
        let projection = rebuild(&events).unwrap();
        let score = projection
            .leads
            .get(&lead_id)
            .unwrap()
            .latest_score
            .as_ref()
            .unwrap();
        assert_eq!(score.revision, 2);
        assert_eq!(score.composite, 80);
    }

    #[test]
    fn malformed_scored_payload_is_hard_error() {
        // Strict, like the ingest payloads: corruption is not a None.
        let lead_id = Uuid::now_v7();
        let events = vec![
            envelope(
                lead_id,
                1,
                event_type::INGESTED,
                ingested_payload(None, Some("url:https://example.com/j"), None),
            ),
            envelope(
                lead_id,
                2,
                event_type::SCORED,
                serde_json::json!({"composite": "high"}),
            ),
        ];
        assert!(rebuild(&events).is_err());
    }

    #[test]
    fn outcome_event_sets_latest_outcome() {
        let lead_id = Uuid::now_v7();
        let events = vec![
            envelope(
                lead_id,
                1,
                event_type::INGESTED,
                ingested_payload(None, Some("url:https://example.com/j"), None),
            ),
            envelope(
                lead_id,
                2,
                event_type::APPLIED,
                serde_json::json!({"method": "manual", "note": "applied via portal"}),
            ),
        ];
        let projection = rebuild(&events).unwrap();
        let record = projection.leads.get(&lead_id).unwrap();
        let outcome = record.latest_outcome.as_ref().unwrap();
        assert_eq!(outcome.event_type, "applied");
        assert_eq!(outcome.note.as_deref(), Some("applied via portal"));
    }

    #[test]
    fn later_outcome_wins() {
        // Latest-wins: a subsequent outcome replaces the previous one.
        let lead_id = Uuid::now_v7();
        let events = vec![
            envelope(
                lead_id,
                1,
                event_type::INGESTED,
                ingested_payload(None, Some("url:https://example.com/j"), None),
            ),
            envelope(lead_id, 2, event_type::APPLIED, serde_json::json!({})),
            envelope(lead_id, 3, event_type::ACCEPTED, serde_json::json!({})),
        ];
        let projection = rebuild(&events).unwrap();
        let record = projection.leads.get(&lead_id).unwrap();
        assert_eq!(
            record.latest_outcome.as_ref().unwrap().event_type,
            "accepted"
        );
    }

    #[test]
    fn older_retro_dated_outcome_does_not_overwrite_newer() {
        // Copilot: `--at` supports backfilling — a retro outcome recorded
        // LATER (replay order) but with an EARLIER occurred_at must not
        // become the projected latest outcome. Latest-wins is chronological,
        // with replay order only as the tie-breaker.
        let lead_id = Uuid::now_v7();
        let mut applied = envelope(
            lead_id,
            2,
            event_type::APPLIED,
            serde_json::json!({"method": "manual"}),
        );
        applied.occurred_at = "2026-08-20T00:00:00Z".parse::<Timestamp>().unwrap();
        let mut screened = envelope(lead_id, 3, event_type::SCREENED, serde_json::json!({}));
        screened.occurred_at = "2026-08-01T00:00:00Z".parse::<Timestamp>().unwrap();
        let events = vec![
            envelope(
                lead_id,
                1,
                event_type::INGESTED,
                ingested_payload(None, Some("url:https://example.com/j"), None),
            ),
            applied,
            screened,
        ];
        let projection = rebuild(&events).unwrap();
        let record = projection.leads.get(&lead_id).unwrap();
        assert_eq!(
            record.latest_outcome.as_ref().unwrap().event_type,
            "applied"
        );
    }

    #[test]
    fn retro_dated_outcome_surfaces_occurred_at() {
        let lead_id = Uuid::now_v7();
        let at = "2026-08-01T00:00:00Z".parse::<Timestamp>().unwrap();
        let mut applied = envelope(
            lead_id,
            2,
            event_type::APPLIED,
            serde_json::json!({"method": "manual"}),
        );
        applied.occurred_at = at;
        let events = vec![
            envelope(
                lead_id,
                1,
                event_type::INGESTED,
                ingested_payload(None, Some("url:https://example.com/j"), None),
            ),
            applied,
        ];
        let projection = rebuild(&events).unwrap();
        let outcome = projection
            .leads
            .get(&lead_id)
            .unwrap()
            .latest_outcome
            .as_ref()
            .unwrap();
        assert_eq!(outcome.occurred_at, at);
    }

    #[test]
    fn malformed_outcome_payload_is_hard_error() {
        // Strict, like the other source-of-truth payloads.
        let lead_id = Uuid::now_v7();
        let events = vec![
            envelope(
                lead_id,
                1,
                event_type::INGESTED,
                ingested_payload(None, Some("url:https://example.com/j"), None),
            ),
            envelope(
                lead_id,
                2,
                event_type::APPLIED,
                serde_json::json!({"note": 5}),
            ),
        ];
        assert!(rebuild(&events).is_err());
    }

    // ── lookup precedence (design doc §2) ────────────────────────

    #[test]
    fn tc_only_matches_when_no_strong_identifier() {
        let lead_id = Uuid::now_v7();
        let events = vec![envelope(
            lead_id,
            1,
            event_type::INGESTED,
            ingested_payload(None, Some("url:https://example.com/j"), Some("tc:abc")),
        )];
        let projection = rebuild(&events).unwrap();

        // Incoming with a strong identifier must NOT merge via tc.
        let with_strong = LeadIdentity {
            dedupe_key: "req:acme:r-9".into(),
            identifiers: Identifiers {
                req: Some("req:acme:r-9".into()),
                url: None,
                tc: Some("tc:abc".into()),
            },
        };
        assert_eq!(projection.lookup(&with_strong), None);

        // Incoming with only tc may match via tc.
        let tc_only = LeadIdentity {
            dedupe_key: "tc:abc".into(),
            identifiers: Identifiers {
                req: None,
                url: None,
                tc: Some("tc:abc".into()),
            },
        };
        assert_eq!(projection.lookup(&tc_only), Some(lead_id));
    }

    #[test]
    fn req_miss_falls_through_to_url_hit() {
        let lead_id = Uuid::now_v7();
        let events = vec![envelope(
            lead_id,
            1,
            event_type::INGESTED,
            ingested_payload(None, Some("url:https://example.com/j"), None),
        )];
        let projection = rebuild(&events).unwrap();
        // Posting gained a req id on repost; still matches via URL index.
        let identity = LeadIdentity {
            dedupe_key: "req:acme:r-1".into(),
            identifiers: Identifiers {
                req: Some("req:acme:r-1".into()),
                url: Some("url:https://example.com/j".into()),
                tc: None,
            },
        };
        assert_eq!(projection.lookup(&identity), Some(lead_id));
    }

    #[test]
    fn req_hit_wins_over_different_url() {
        // A lead indexed with req:A and url:X; an incoming posting carrying
        // req:A and a *different* url:Y must match via req (precedence
        // order), not fall through to a url miss.
        let lead_id = Uuid::now_v7();
        let events = vec![envelope(
            lead_id,
            1,
            event_type::INGESTED,
            ingested_payload(
                Some("req:acme:r-1"),
                Some("url:https://example.com/x"),
                None,
            ),
        )];
        let projection = rebuild(&events).unwrap();
        let identity = LeadIdentity {
            dedupe_key: "req:acme:r-1".into(),
            identifiers: Identifiers {
                req: Some("req:acme:r-1".into()),
                url: Some("url:https://example.com/y".into()),
                tc: None,
            },
        };
        assert_eq!(projection.lookup(&identity), Some(lead_id));
    }

    #[test]
    fn raw_fallback_key_is_matchable_on_reingest() {
        // A posting with no req/url/title/company falls back to a `raw:`
        // dedupe key (identity.rs). That key is stored in `dedupe_key` but
        // never indexed, so re-ingesting the same unstructured drop mints a
        // new lead. Either index the `raw:` form or reject such postings;
        // this test documents the "index it" intent.
        let lead_id = Uuid::now_v7();
        let events = vec![envelope(
            lead_id,
            1,
            event_type::INGESTED,
            serde_json::json!({
                "dedupe_key": "raw:abc123",
                "identifiers": {},
                "source": "drop-in",
                "raw_text": "unstructured body",
                "extracted": {}
            }),
        )];
        let projection = rebuild(&events).unwrap();
        let identity = LeadIdentity {
            dedupe_key: "raw:abc123".into(),
            identifiers: Identifiers::default(),
        };
        assert_eq!(projection.lookup(&identity), Some(lead_id));
    }

    // ── id prefix addressing ─────────────────────────────────────

    #[test]
    fn find_by_id_prefix() {
        let lead_id = Uuid::now_v7();
        let events = vec![envelope(
            lead_id,
            1,
            event_type::INGESTED,
            ingested_payload(None, None, Some("tc:abc")),
        )];
        let projection = rebuild(&events).unwrap();
        let prefix = &lead_id.to_string()[..8];
        assert_eq!(projection.find_by_id_prefix(prefix).len(), 1);
        assert!(projection.find_by_id_prefix("zzzzzzzz").is_empty());
    }

    #[test]
    fn find_by_id_prefix_returns_all_matches() {
        // Two leads sharing a prefix must both be returned so the caller can
        // report ambiguity (design doc §8: "unambiguous UUID prefix").
        let a = Uuid::parse_str("0192f8a1-0000-7000-8000-000000000001").unwrap();
        let b = Uuid::parse_str("0192f8a1-0000-7000-8000-000000000002").unwrap();
        let events = vec![
            envelope(
                a,
                1,
                event_type::INGESTED,
                ingested_payload(None, None, Some("tc:a")),
            ),
            envelope(
                b,
                1,
                event_type::INGESTED,
                ingested_payload(None, None, Some("tc:b")),
            ),
        ];
        let projection = rebuild(&events).unwrap();
        let matches = projection.find_by_id_prefix("0192f8a1");
        assert_eq!(matches.len(), 2);
        let ids: Vec<Uuid> = matches.iter().map(|r| r.lead_id).collect();
        assert!(ids.contains(&a));
        assert!(ids.contains(&b));
    }
}
