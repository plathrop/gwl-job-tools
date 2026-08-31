//! In-memory projections, rebuilt by replaying the log at startup (design
//! doc 0001 §7): the LeadIndex (identifier forms → lead_id, drives §2
//! matching) and the LeadBook (per-lead latest snapshot).

use std::collections::HashMap;

use jiff::Timestamp;
use miette::{IntoDiagnostic, Result};
use serde::Serialize;
use serde_with::skip_serializing_none;
use uuid::Uuid;

use crate::domain::{
    events::{
        ApplyQueuedPayload, EventEnvelope, ExtractedFields, Identifiers, OutcomePayload,
        ReviewedPayload, ScoredPayload, event_type,
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
    pub adapter: Option<String>,
    /// How the lead was found (`search`/`recruiter`/`referrer`/`unknown`).
    pub source: Option<String>,
    pub url: Option<String>,
    pub extracted: ExtractedFields,
    pub latest_mark: Option<String>,
    /// Number of times the lead was marked `defer` (design doc §5).
    pub deferral_count: u64,
    /// Whether an `apply_queued` event followed the latest
    /// `apply-automatically` mark (pending-recovery rule, design doc §7).
    pub apply_queued: bool,
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
    /// How an `applied` application was submitted (`manual`/
    /// `auto-assisted`); absent on every other outcome type and when the
    /// user didn't pass `--method`.
    pub method: Option<String>,
    pub occurred_at: Timestamp,
}

impl LeadRecord {
    /// The derived lifecycle status (design doc 0002, decision record 0010):
    /// the single application-stage dimension the user sees — computed
    /// from the underlying facts, never stored. The latest outcome (a
    /// fact) wins; without one, the latest mark (a decision) stands in;
    /// with neither, the queue state names the stage. Marks and outcomes
    /// remain distinct events in the log; this is the view over them.
    pub fn lifecycle_status(&self) -> String {
        if let Some(outcome) = &self.latest_outcome {
            if outcome.event_type == event_type::APPLIED {
                // The automation bit prefers the recorded fact, falling
                // back to what the decision mark implies (design doc 0002).
                let method = outcome.method.as_deref().or_else(|| self.mark_method());
                return match method {
                    Some(method) => format!("applied ({method})"),
                    None => "applied".into(),
                };
            }
            return outcome.event_type.clone();
        }
        match self.latest_mark.as_deref() {
            Some("apply-automatically") => "applying (auto-assisted)".into(),
            Some("apply-manual") => "applying (manual)".into(),
            Some("defer") => "deferred".into(),
            Some("ignore") => "ignored".into(),
            // Unmarked: the queue state names the stage — scored means
            // pending review; a standing rejection means it never reached
            // the queue at the latest evaluation.
            _ => {
                if self.latest_score.is_some() {
                    "pending".into()
                } else if self.latest_rejection.is_some() {
                    "rejected".into()
                } else {
                    "ingested".into()
                }
            }
        }
    }

    /// The submission method the lead's apply mark implies, for defaulting
    /// `applied --method` and decorating the status (design doc 0002): the
    /// mark recorded which flow was chosen, so the outcome need not repeat
    /// it unless the user wants to correct the record. Single source of
    /// truth for the mapping — `resolve_apply_method` delegates here (PR
    /// #16 review: the map must not drift).
    pub fn mark_method(&self) -> Option<&'static str> {
        match self.latest_mark.as_deref() {
            Some("apply-automatically") => Some("auto-assisted"),
            Some("apply-manual") => Some("manual"),
            _ => None,
        }
    }

    /// Whether the lead is durably ignored (design doc 0001 §2): the
    /// latest mark is `ignore`, which exists to bury the lead permanently.
    /// Ignored leads appear only in `list --all`, never in the active
    /// pipeline (design doc 0002) — and never in the review queue (§7).
    pub fn is_buried(&self) -> bool {
        self.latest_mark.as_deref() == Some("ignore")
    }

    /// Whether the lead has reached a terminal state: its latest outcome is
    /// one of the terminal types (design doc 0001 §3). The latest-wins
    /// resolution happens in `rebuild`'s `is_newer` guard — this method
    /// only reads the already-resolved `latest_outcome` — so a later
    /// non-terminal outcome (e.g. `applied` recorded after an `archived`)
    /// un-terminals the lead.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self.latest_outcome.as_ref().map(|o| o.event_type.as_str()),
            Some(
                event_type::ACCEPTED
                    | event_type::REJECTED_BY_EMPLOYER
                    | event_type::WITHDRAWN
                    | event_type::DECLINED
                    | event_type::UNRESPONSIVE
                    | event_type::ARCHIVED
            )
        )
    }
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

    /// Which lead owns an identifier form (`req:`/`url:`/`tc:`/`raw:` key or
    /// dedupe key), if any. The edit command uses this to refuse a
    /// correction that would collide with a different lead's identity
    /// (decision record 0009): identifiers are indexed additively, never
    /// re-pointed.
    pub fn identifier_owner(&self, form: &str) -> Option<Uuid> {
        self.req_index
            .get(form)
            .or_else(|| self.url_index.get(form))
            .or_else(|| self.tc_index.get(form))
            .or_else(|| self.dedupe_index.get(form))
            .copied()
    }

    /// All leads, ranked by composite score descending with first-seen as
    /// the deterministic tie-breaker (design doc 0002). The shared ranking
    /// for `list`'s default and `--all` views — one sort, so the two
    /// cannot drift (PR #16 review).
    pub fn ranked_leads(&self) -> Vec<&LeadRecord> {
        let mut leads: Vec<&LeadRecord> = self.leads.values().collect();
        leads.sort_by(|a, b| {
            let sa = a.latest_score.as_ref().map(|s| s.composite).unwrap_or(0);
            let sb = b.latest_score.as_ref().map(|s| s.composite).unwrap_or(0);
            sb.cmp(&sa).then_with(|| a.first_seen.cmp(&b.first_seen))
        });
        leads
    }

    /// The active pipeline (design doc 0002, decision record 0010): every
    /// lead that has neither reached a terminal state nor been durably
    /// ignored. This is `list`'s default view — the pending review queue
    /// (§7) remains what `review` steps through, and remains a subset of
    /// this. Same ranking as `--all` (via `ranked_leads`). Ignored leads
    /// are excluded because the ignore mark exists to bury leads
    /// permanently (`--all` reveals them). Gate-rejected leads ARE
    /// included: a machine rejection is not a terminal state, the leads
    /// sort to the bottom with a `[rejected]` tag, and they are `edit`-
    /// revivable (decision record 0010).
    pub fn active_leads(&self) -> Vec<&LeadRecord> {
        let mut active = self.ranked_leads();
        active.retain(|r| !r.is_terminal() && !r.is_buried());
        active
    }

    /// The pending review queue (design doc §7): leads with a current
    /// `scored`, no outcome event, and latest mark absent or `defer` — plus
    /// the pending-recovery rule (an `apply-automatically` mark with no
    /// `apply_queued` is still pending). Ranked by composite score
    /// descending.
    pub fn pending_queue(&self) -> Vec<&LeadRecord> {
        let mut pending: Vec<&LeadRecord> = self
            .leads
            .values()
            .filter(|r| {
                r.latest_score.is_some()
                    && r.latest_outcome.is_none()
                    && match r.latest_mark.as_deref() {
                        None | Some("defer") => true,
                        Some("apply-automatically") => !r.apply_queued,
                        _ => false,
                    }
            })
            .collect();
        pending.sort_by(|a, b| {
            let sa = a.latest_score.as_ref().map(|s| s.composite).unwrap_or(0);
            let sb = b.latest_score.as_ref().map(|s| s.composite).unwrap_or(0);
            sb.cmp(&sa).then_with(|| a.first_seen.cmp(&b.first_seen))
        });
        pending
    }
}

#[derive(serde::Deserialize)]
struct IngestView {
    dedupe_key: String,
    #[serde(default)]
    identifiers: Identifiers,
    adapter: String,
    #[serde(default = "crate::domain::events::default_lead_source")]
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
            event_type::INGESTED | event_type::UPDATED | event_type::EDITED => {
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
                        adapter: None,
                        source: None,
                        url: None,
                        extracted: ExtractedFields::default(),
                        latest_mark: None,
                        deferral_count: 0,
                        apply_queued: false,
                        latest_rejection: None,
                        latest_score: None,
                        latest_outcome: None,
                        event_count: 0,
                        first_seen: event.recorded_at,
                        last_event: event.recorded_at,
                    });
                record.dedupe_key = Some(view.dedupe_key);
                record.identifiers = view.identifiers;
                record.adapter = Some(view.adapter);
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
                // Strict, like the ingest payloads: a mistyped mark is
                // source-of-truth corruption, not a None (a lenient decode
                // would silently clear the mark and queue state).
                let payload: ReviewedPayload = serde_json::from_value(event.payload.clone())
                    .into_diagnostic()
                    .map_err(|e| {
                        e.wrap_err(format!(
                            "decoding reviewed payload of event {} (seq {})",
                            event.id, event.seq
                        ))
                    })?;
                if let Some(record) = projection.leads.get_mut(&lead_id) {
                    record.latest_mark = Some(payload.mark);
                    if record.latest_mark.as_deref() == Some("defer") {
                        record.deferral_count += 1;
                    }
                    // A new mark invalidates any prior apply_queued (the
                    // package belonged to the previous mark).
                    record.apply_queued = false;
                    record.event_count += 1;
                    record.last_event = event.recorded_at;
                }
            }
            event_type::APPLY_QUEUED => {
                // Strict decode: a malformed apply_queued is corruption, not
                // a None — otherwise a syntactically valid but malformed
                // event would silently drop a lead from the recovery queue.
                let _: ApplyQueuedPayload = serde_json::from_value(event.payload.clone())
                    .into_diagnostic()
                    .map_err(|e| {
                        e.wrap_err(format!(
                            "decoding apply_queued payload of event {} (seq {})",
                            event.id, event.seq
                        ))
                    })?;
                if let Some(record) = projection.leads.get_mut(&lead_id) {
                    record.apply_queued = true;
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
                    // Retro-dated events must not regress the projected state:
                    // recording `screened --at <older>` after `accepted` must
                    // not make `show` report `screened`. Only replace when the
                    // new event's `occurred_at` is at least as recent (replay
                    // order is the tie-breaker, since `>=` replaces on equal).
                    let is_newer = record
                        .latest_outcome
                        .as_ref()
                        .is_none_or(|current| event.occurred_at >= current.occurred_at);
                    if is_newer {
                        record.latest_outcome = Some(OutcomeView {
                            event_type: event.event_type.clone(),
                            note: payload.note,
                            method: payload.method,
                            occurred_at: event.occurred_at,
                        });
                    }
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
    use serde_json::Value;

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

    /// Like `envelope`, but with an explicit `recorded_at`/`occurred_at` so
    /// tests can control `first_seen` ordering.
    fn envelope_at(
        lead_id: Uuid,
        seq: u64,
        event_type: &str,
        payload: serde_json::Value,
        at: Timestamp,
    ) -> EventEnvelope {
        let mut e = envelope(lead_id, seq, event_type, payload);
        e.occurred_at = at;
        e.recorded_at = at;
        e
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
            "adapter": "drop-in",
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
    fn edited_event_refreshes_snapshot_and_additively_indexes() {
        // Decision record 0009: `edited` is a snapshot event (refresh the
        // record) and its recomputed identifiers are indexed WITHOUT
        // dropping the old forms — a future posting matching either the
        // old or the new identity must find the lead.
        let lead_id = Uuid::now_v7();
        let events = vec![
            envelope(
                lead_id,
                1,
                event_type::INGESTED,
                ingested_payload(None, Some("url:https://example.com/j"), Some("tc:old")),
            ),
            envelope(
                lead_id,
                2,
                event_type::EDITED,
                serde_json::json!({
                    "dedupe_key": "url:https://example.com/j",
                    "identifiers": {
                        "url": "url:https://example.com/j",
                        "tc": "tc:new"
                    },
                    "changed": ["title", "company"],
                    "adapter": "user",
                    "source": "unknown",
                    "raw_text": "body",
                    "extracted": {"title": "Senior Engineer", "company": "Acme Corp"}
                }),
            ),
        ];
        let projection = rebuild(&events).unwrap();
        let record = projection.leads.get(&lead_id).unwrap();
        assert_eq!(record.extracted.title.as_deref(), Some("Senior Engineer"));
        assert_eq!(record.adapter.as_deref(), Some("user"));
        assert_eq!(record.event_count, 2);

        // Additive: both the old and the recomputed tc forms match.
        let old = LeadIdentity {
            dedupe_key: "tc:old".into(),
            identifiers: Identifiers {
                tc: Some("tc:old".into()),
                ..Default::default()
            },
        };
        let new = LeadIdentity {
            dedupe_key: "tc:new".into(),
            identifiers: Identifiers {
                tc: Some("tc:new".into()),
                ..Default::default()
            },
        };
        assert_eq!(projection.lookup(&old), Some(lead_id));
        assert_eq!(projection.lookup(&new), Some(lead_id));
    }

    #[test]
    fn identifier_owner_finds_forms_across_indexes() {
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
        assert_eq!(projection.identifier_owner("req:acme:r-1"), Some(lead_id));
        assert_eq!(
            projection.identifier_owner("url:https://example.com/j"),
            Some(lead_id)
        );
        assert_eq!(projection.identifier_owner("tc:abc"), Some(lead_id));
        assert_eq!(projection.identifier_owner("tc:missing"), None);
    }

    #[test]
    fn edited_snapshot_invalidates_stale_score() {
        // The pass-marker rule (decision 0006) covers `edited` like every
        // snapshot event.
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
                event_type::EDITED,
                serde_json::json!({
                    "dedupe_key": "url:https://example.com/j",
                    "identifiers": {"url": "url:https://example.com/j"},
                    "changed": ["remote"],
                    "adapter": "user",
                    "source": "unknown",
                    "raw_text": "body",
                    "extracted": {"title": "Engineer", "company": "Acme", "remote": true}
                }),
            ),
        ];
        let projection = rebuild(&events).unwrap();
        assert!(
            projection
                .leads
                .get(&lead_id)
                .unwrap()
                .latest_score
                .is_none()
        );
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
                    "adapter": "drop-in",
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
                    "adapter": "drop-in",
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
            "adapter": "drop-in",
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
                "adapter": "drop-in",
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

    // ── review queue (Increment 4a) ───────────────────────────────

    fn reviewed_payload(mark: &str) -> Value {
        serde_json::json!({ "mark": mark })
    }

    fn apply_queued_payload() -> Value {
        serde_json::json!({
            "package": {
                "cover_letter_path": "/tmp/letter.pdf",
                "resume_path": "/tmp/resume.pdf",
                "cheat_sheet": []
            },
            "url": "https://example.com/j"
        })
    }

    /// A lead that has been ingested and scored (the queue-membership
    /// prerequisite).
    fn scored_lead(lead_id: Uuid) -> Vec<EventEnvelope> {
        vec![
            envelope(
                lead_id,
                1,
                event_type::INGESTED,
                ingested_payload(None, Some("url:https://example.com/j"), None),
            ),
            envelope(lead_id, 2, event_type::SCORED, scored_payload(75, 1)),
        ]
    }

    #[test]
    fn deferral_count_increments_per_defer_mark() {
        let lead_id = Uuid::now_v7();
        let mut events = scored_lead(lead_id);
        events.push(envelope(
            lead_id,
            3,
            event_type::REVIEWED,
            reviewed_payload("defer"),
        ));
        events.push(envelope(
            lead_id,
            4,
            event_type::REVIEWED,
            reviewed_payload("defer"),
        ));
        let projection = rebuild(&events).unwrap();
        let record = projection.leads.get(&lead_id).unwrap();
        assert_eq!(record.deferral_count, 2);
        assert_eq!(record.latest_mark.as_deref(), Some("defer"));
    }

    // ── derived lifecycle status (design doc 0002) ───────────

    #[test]
    fn lifecycle_status_covers_the_stages() {
        let lead_id = Uuid::now_v7();

        // Scored, no mark: pending.
        let projection = rebuild(&scored_lead(lead_id)).unwrap();
        assert_eq!(projection.leads[&lead_id].lifecycle_status(), "pending");

        // Gate-rejected at the latest evaluation (no standing score):
        // rejected.
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
                event_type::REJECTED,
                serde_json::json!({"gate": "remote-only", "reason": "x", "revision": 1}),
            ),
        ];
        assert_eq!(
            rebuild(&events).unwrap().leads[&lead_id].lifecycle_status(),
            "rejected"
        );

        // Decisions without recorded outcomes.
        let mut events = scored_lead(lead_id);
        events.push(envelope(
            lead_id,
            3,
            event_type::REVIEWED,
            reviewed_payload("apply-manual"),
        ));
        assert_eq!(
            rebuild(&events).unwrap().leads[&lead_id].lifecycle_status(),
            "applying (manual)"
        );

        let mut events = scored_lead(lead_id);
        events.push(envelope(
            lead_id,
            3,
            event_type::REVIEWED,
            reviewed_payload("apply-automatically"),
        ));
        events.push(envelope(
            lead_id,
            4,
            event_type::APPLY_QUEUED,
            apply_queued_payload(),
        ));
        assert_eq!(
            rebuild(&events).unwrap().leads[&lead_id].lifecycle_status(),
            "applying (auto-assisted)"
        );

        let mut events = scored_lead(lead_id);
        events.push(envelope(
            lead_id,
            3,
            event_type::REVIEWED,
            reviewed_payload("defer"),
        ));
        assert_eq!(
            rebuild(&events).unwrap().leads[&lead_id].lifecycle_status(),
            "deferred"
        );

        let mut events = scored_lead(lead_id);
        events.push(envelope(
            lead_id,
            3,
            event_type::REVIEWED,
            reviewed_payload("ignore"),
        ));
        assert_eq!(
            rebuild(&events).unwrap().leads[&lead_id].lifecycle_status(),
            "ignored"
        );
    }

    #[test]
    fn lifecycle_status_applied_method_prefers_fact_then_mark() {
        // The recorded fact wins: an explicit --method on the outcome.
        let lead_id = Uuid::now_v7();
        let mut events = scored_lead(lead_id);
        events.push(envelope(
            lead_id,
            3,
            event_type::APPLIED,
            serde_json::json!({"method": "manual"}),
        ));
        let record = &rebuild(&events).unwrap().leads[&lead_id];
        assert_eq!(record.lifecycle_status(), "applied (manual)");
        assert!(!record.is_terminal());

        // No method on the outcome: derive it from the decision mark
        // (design doc 0002 — the mark recorded which flow was chosen).
        let mut events = scored_lead(lead_id);
        events.push(envelope(
            lead_id,
            3,
            event_type::REVIEWED,
            reviewed_payload("apply-automatically"),
        ));
        events.push(envelope(
            lead_id,
            4,
            event_type::APPLY_QUEUED,
            apply_queued_payload(),
        ));
        events.push(envelope(
            lead_id,
            5,
            event_type::APPLIED,
            serde_json::json!({}),
        ));
        assert_eq!(
            rebuild(&events).unwrap().leads[&lead_id].lifecycle_status(),
            "applied (auto-assisted)"
        );

        // Neither fact nor mark: plain applied.
        let mut events = scored_lead(lead_id);
        events.push(envelope(
            lead_id,
            3,
            event_type::APPLIED,
            serde_json::json!({}),
        ));
        assert_eq!(
            rebuild(&events).unwrap().leads[&lead_id].lifecycle_status(),
            "applied"
        );
    }

    #[test]
    fn lifecycle_status_terminal_outcome_and_is_terminal() {
        let lead_id = Uuid::now_v7();
        let mut events = scored_lead(lead_id);
        events.push(envelope(
            lead_id,
            3,
            event_type::REJECTED_BY_EMPLOYER,
            serde_json::json!({"note": "gone with the req"}),
        ));
        let record = &rebuild(&events).unwrap().leads[&lead_id];
        assert_eq!(record.lifecycle_status(), "rejected_by_employer");
        assert!(record.is_terminal());

        // A later non-terminal outcome un-terminals (latest-wins by
        // occurred_at): archived then re-applied later is active again.
        let archived = {
            let mut e = envelope(lead_id, 3, event_type::ARCHIVED, serde_json::json!({}));
            e.occurred_at = Timestamp::from_second(1_700_000_000).unwrap();
            e
        };
        let reapplied = {
            let mut e = envelope(
                lead_id,
                4,
                event_type::APPLIED,
                serde_json::json!({"method": "manual"}),
            );
            e.occurred_at = Timestamp::from_second(1_700_100_000).unwrap();
            e
        };
        let mut events = scored_lead(lead_id);
        events.push(archived);
        events.push(reapplied);
        let record = &rebuild(&events).unwrap().leads[&lead_id];
        assert!(!record.is_terminal());
        assert_eq!(record.lifecycle_status(), "applied (manual)");
    }

    #[test]
    fn outcome_view_carries_method() {
        let lead_id = Uuid::now_v7();
        let mut events = scored_lead(lead_id);
        events.push(envelope(
            lead_id,
            3,
            event_type::APPLIED,
            serde_json::json!({"method": "auto-assisted"}),
        ));
        let record = &rebuild(&events).unwrap().leads[&lead_id];
        assert_eq!(
            record.latest_outcome.as_ref().unwrap().method.as_deref(),
            Some("auto-assisted")
        );
    }

    #[test]
    fn lifecycle_status_pins_remaining_rule_table_rows() {
        // The rule table is the contract (design doc 0002); pin the rows
        // the stage-walk test doesn't reach (PR #16 review).
        let lead_id = Uuid::now_v7();

        // Non-terminal outcome stages pass through by name.
        for stage in ["screened", "interviewed", "offered"] {
            let mut events = scored_lead(lead_id);
            events.push(envelope(lead_id, 3, stage, serde_json::json!({})));
            assert_eq!(
                rebuild(&events).unwrap().leads[&lead_id].lifecycle_status(),
                stage,
                "stage {stage}"
            );
        }

        // Torn-batch edge: an ingested snapshot whose evaluation events
        // were lost has neither a standing score nor a rejection — the
        // last-resort row of the table.
        let events = vec![envelope(
            lead_id,
            1,
            event_type::INGESTED,
            ingested_payload(None, Some("url:https://example.com/j"), None),
        )];
        assert_eq!(
            rebuild(&events).unwrap().leads[&lead_id].lifecycle_status(),
            "ingested"
        );
    }

    #[test]
    fn active_leads_is_the_non_terminal_unignored_pipeline() {
        // `list`'s default view (decision record 0010): every lead that has
        // neither reached a terminal state nor been durably ignored —
        // pending, deferred, applying, applied — sorted by score. The
        // ignore mark buries a lead permanently: `--all` is the only view
        // that reveals it (settled with Grey 2026-08-31).
        let a = Uuid::now_v7(); // scored, pending
        let b = Uuid::now_v7(); // applied
        let c = Uuid::now_v7(); // terminal (declined)
        let d = Uuid::now_v7(); // ignored (buried)
        let t1 = Timestamp::from_second(1_700_000_000).unwrap();
        let t2 = Timestamp::from_second(1_700_000_001).unwrap();
        let mut events = vec![
            envelope_at(
                a,
                1,
                event_type::INGESTED,
                ingested_payload(None, Some("url:https://example.com/a"), None),
                t1,
            ),
            envelope_at(a, 2, event_type::SCORED, scored_payload(75, 1), t1),
            envelope_at(
                b,
                1,
                event_type::INGESTED,
                ingested_payload(None, Some("url:https://example.com/b"), None),
                t2,
            ),
            envelope_at(b, 2, event_type::SCORED, scored_payload(90, 1), t2),
            envelope_at(
                b,
                3,
                event_type::APPLIED,
                serde_json::json!({"method": "manual"}),
                t2,
            ),
            envelope_at(
                c,
                1,
                event_type::INGESTED,
                ingested_payload(None, Some("url:https://example.com/c"), None),
                t2,
            ),
            envelope_at(c, 2, event_type::SCORED, scored_payload(50, 1), t2),
            envelope_at(c, 3, event_type::DECLINED, serde_json::json!({}), t2),
        ];
        events.extend(scored_lead(d));
        events.push(envelope(
            d,
            3,
            event_type::REVIEWED,
            reviewed_payload("ignore"),
        ));

        let projection = rebuild(&events).unwrap();
        let active = projection.active_leads();
        let ids: Vec<Uuid> = active.iter().map(|r| r.lead_id).collect();
        // Terminal c and ignored d are excluded; b (90) ranks above a (75).
        assert_eq!(ids, vec![b, a]);
    }

    #[test]
    fn pending_queue_includes_scored_unmarked_leads() {
        let lead_id = Uuid::now_v7();
        let projection = rebuild(&scored_lead(lead_id)).unwrap();
        let pending = projection.pending_queue();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].lead_id, lead_id);
    }

    #[test]
    fn pending_queue_excludes_marked_and_outcome_leads() {
        let a = Uuid::now_v7();
        let b = Uuid::now_v7();
        let c = Uuid::now_v7();
        let mut events = scored_lead(a);
        // b is marked apply-manual (acted on, not pending).
        events.extend(scored_lead(b));
        events.push(envelope(
            b,
            3,
            event_type::REVIEWED,
            reviewed_payload("apply-manual"),
        ));
        // c has an outcome (applied).
        events.extend(scored_lead(c));
        events.push(envelope(
            c,
            3,
            event_type::APPLIED,
            serde_json::json!({"method": "manual"}),
        ));
        let projection = rebuild(&events).unwrap();
        let pending = projection.pending_queue();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].lead_id, a);
    }

    #[test]
    fn apply_automatically_without_apply_queued_is_still_pending() {
        // Pending-recovery rule (design doc §7): a crash mid-batch leaves the
        // reviewed{apply-automatically} without its apply_queued; the lead
        // must stay pending.
        let lead_id = Uuid::now_v7();
        let mut events = scored_lead(lead_id);
        events.push(envelope(
            lead_id,
            3,
            event_type::REVIEWED,
            reviewed_payload("apply-automatically"),
        ));
        let projection = rebuild(&events).unwrap();
        assert_eq!(projection.pending_queue().len(), 1);
    }

    #[test]
    fn apply_automatically_with_apply_queued_leaves_the_queue() {
        let lead_id = Uuid::now_v7();
        let mut events = scored_lead(lead_id);
        events.push(envelope(
            lead_id,
            3,
            event_type::REVIEWED,
            reviewed_payload("apply-automatically"),
        ));
        events.push(envelope(
            lead_id,
            4,
            event_type::APPLY_QUEUED,
            apply_queued_payload(),
        ));
        let projection = rebuild(&events).unwrap();
        assert!(projection.pending_queue().is_empty());
    }

    #[test]
    fn malformed_reviewed_payload_is_hard_error() {
        // A lenient decode would silently clear the mark and queue state;
        // the log is the source of truth, so a mistyped mark is corruption.
        let lead_id = Uuid::now_v7();
        let mut events = scored_lead(lead_id);
        events.push(envelope(
            lead_id,
            3,
            event_type::REVIEWED,
            serde_json::json!({}),
        ));
        assert!(rebuild(&events).is_err());
    }

    #[test]
    fn malformed_apply_queued_payload_is_hard_error() {
        // A malformed apply_queued must not silently drop a lead from the
        // recovery queue.
        let lead_id = Uuid::now_v7();
        let mut events = scored_lead(lead_id);
        events.push(envelope(
            lead_id,
            3,
            event_type::REVIEWED,
            reviewed_payload("apply-automatically"),
        ));
        events.push(envelope(
            lead_id,
            4,
            event_type::APPLY_QUEUED,
            serde_json::json!({}),
        ));
        assert!(rebuild(&events).is_err());
    }

    #[test]
    fn pending_queue_tie_breaks_on_first_seen() {
        // Composite is 0–100, so ties are routine; the tie-breaker must be
        // deterministic (first-seen ascending), not HashMap iteration order.
        let a = Uuid::now_v7();
        let b = Uuid::now_v7();
        let t1 = Timestamp::from_second(1_700_000_000).unwrap();
        let t2 = Timestamp::from_second(1_700_000_001).unwrap();
        let events = vec![
            envelope_at(
                a,
                1,
                event_type::INGESTED,
                ingested_payload(None, Some("url:https://example.com/a"), None),
                t1,
            ),
            envelope_at(a, 2, event_type::SCORED, scored_payload(75, 1), t1),
            envelope_at(
                b,
                1,
                event_type::INGESTED,
                ingested_payload(None, Some("url:https://example.com/b"), None),
                t2,
            ),
            envelope_at(b, 2, event_type::SCORED, scored_payload(75, 1), t2),
        ];
        let projection = rebuild(&events).unwrap();
        let pending = projection.pending_queue();
        assert_eq!(pending.len(), 2);
        assert_eq!(pending[0].lead_id, a);
        assert_eq!(pending[1].lead_id, b);
    }
}
