//! The Lead aggregate: pure `decide` / `evolve` over replayed state.
//!
//! Design doc 0001 §2 (identity & re-ingest semantics) and §1 (aggregate
//! pattern). Gates and scoring land in Increments 2–3; the aggregate already
//! understands `reviewed` marks so the durable-ignore suppression rule is in
//! place before marks exist.

use miette::{Result, bail};
use serde_json::Value;
use uuid::Uuid;

use crate::domain::{
    events::{
        ApplyQueuedPayload, EventEnvelope, ExtractedFields, IngestedPayload, PendingEvent,
        ReingestSuppressedPayload, RejectedPayload, ReviewedPayload, ScoredPayload, SnapshotFields,
        UpdatedPayload, event_type,
    },
    gates::GateFailure,
    identity::LeadIdentity,
    scoring::ScoreResult,
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
    /// Count of gate/score evaluations (rejected + scored events).
    pub eval_revision: u64,
    /// The latest `scored` payload, if the lead has ever passed gates.
    pub latest_score: Option<ScoredPayload>,
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
            // Every snapshot event is one gate/score evaluation; rejected
            // and scored events carry that evaluation's revision. Counting
            // evaluations here (not on rejected/scored) keeps the counter
            // correct when an evaluation fails multiple gates (one
            // revision, several events) and when an evaluation passes
            // (snapshot event, no rejected/scored events at all).
            state.eval_revision += 1;
            // A new snapshot invalidates the previous score: the new
            // evaluation's `scored` event (if any) follows in the same batch.
            // If the batch tears, the lead must not present a stale score
            // (decision 0006).
            state.latest_score = None;
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
        event_type::SCORED => {
            if let Ok(score) = serde_json::from_value::<ScoredPayload>(event.payload.clone()) {
                state.latest_score = Some(score);
            }
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
// The argument count is the command input (identity + snapshot + evaluation)
// plus the replayed state; bundling would obscure the decide/evolve seam.
#[allow(clippy::too_many_arguments)]
pub fn decide_ingest(
    state: &LeadState,
    identity: &LeadIdentity,
    adapter: &str,
    source: &str,
    url: Option<String>,
    raw_text: String,
    extracted: ExtractedFields,
    gate_failures: Vec<GateFailure>,
    score: Option<ScoreResult>,
) -> Result<(IngestKind, Vec<PendingEvent>)> {
    // Pass-marker invariant (decision 0006): a non-suppressed evaluation
    // emits exactly one of `rejected` (gate failure) or `scored` (gate pass).
    // The suppressed path (ignore mark) is exempt — it returns before any
    // evaluation events are considered.
    let suppressed = state.exists && state.latest_mark.as_deref() == Some("ignore");
    if !suppressed {
        if !gate_failures.is_empty() && score.is_some() {
            bail!("evaluation cannot be both rejected and scored");
        }
        if gate_failures.is_empty() && score.is_none() {
            bail!("a gate-passing evaluation must carry a score");
        }
    }

    if !state.exists {
        let payload = IngestedPayload {
            dedupe_key: identity.dedupe_key.clone(),
            identifiers: identity.identifiers.clone(),
            snapshot: SnapshotFields {
                adapter: adapter.into(),
                source: source.into(),
                url,
                raw_text: Some(raw_text),
                extracted,
            },
        };
        let mut events = vec![PendingEvent::new(event_type::INGESTED, None, &payload)?];
        events.extend(rejection_events(state, gate_failures)?);
        if let Some(score) = score {
            events.push(scored_event(state, score)?);
        }
        return Ok((IngestKind::New, events));
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
            adapter: adapter.into(),
            source: source.into(),
            url,
            raw_text: Some(raw_text),
            extracted,
        },
    };
    let mut events = vec![PendingEvent::new(event_type::UPDATED, None, &payload)?];
    events.extend(rejection_events(state, gate_failures)?);
    if let Some(score) = score {
        events.push(scored_event(state, score)?);
    }
    Ok((IngestKind::Updated { changed }, events))
}

/// The `scored` event for a gate-passing evaluation, sharing the evaluation's
/// revision. `scored` is the pass-marker (decision 0006): a lead whose latest
/// evaluation passed gates carries a `scored` event.
fn scored_event(state: &LeadState, score: ScoreResult) -> Result<PendingEvent> {
    PendingEvent::new(
        event_type::SCORED,
        None,
        &ScoredPayload {
            composite: score.composite,
            revision: state.eval_revision + 1,
            dimensions: score.dimensions,
            breakdown: score.breakdown,
        },
    )
}

/// One `rejected` event per failed gate, sharing the evaluation's revision
/// (the count of prior gate/score evaluations, +1). Causation within the
/// batch is chained at append time, so these are caused by the snapshot
/// event they follow.
fn rejection_events(
    state: &LeadState,
    gate_failures: Vec<GateFailure>,
) -> Result<Vec<PendingEvent>> {
    gate_failures
        .into_iter()
        .map(|failure| {
            PendingEvent::new(
                event_type::REJECTED,
                None,
                &RejectedPayload {
                    gate: failure.gate.as_str().into(),
                    reason: failure.reason,
                    revision: state.eval_revision + 1,
                },
            )
        })
        .collect()
}

/// Decide the events for a user mark (design doc 0001 §3, §5). Marks are
/// latest-wins; re-marking emits a new `reviewed` event. `apply-automatically`
/// also emits `apply_queued` in the same batch (the mark IS the approval; the
/// package was prepared by the command layer). The pass-marker invariant
/// (decision 0006) analog: `apply-automatically` requires a prepared package,
/// and only `apply-automatically` carries one.
pub fn decide_mark(
    mark: &str,
    note: Option<String>,
    apply: Option<ApplyQueuedPayload>,
) -> Result<Vec<PendingEvent>> {
    const MARKS: [&str; 4] = ["apply-automatically", "apply-manual", "defer", "ignore"];
    if !MARKS.contains(&mark) {
        bail!("unknown mark '{mark}'");
    }
    if (mark == "apply-automatically") != apply.is_some() {
        bail!(
            "apply-automatically requires a prepared package, and only apply-automatically carries one"
        );
    }

    let mut events = vec![PendingEvent::new(
        event_type::REVIEWED,
        None,
        &ReviewedPayload {
            mark: mark.into(),
            note,
        },
    )?];
    if let Some(apply) = apply {
        events.push(PendingEvent::new(event_type::APPLY_QUEUED, None, &apply)?);
    }
    Ok(events)
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

    fn score_result() -> ScoreResult {
        ScoreResult {
            composite: 50,
            dimensions: vec![],
            breakdown: "50".into(),
        }
    }

    fn ingested_envelope() -> EventEnvelope {
        EventEnvelope {
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
                "adapter": "drop-in",
                "raw_text": "body",
                "extracted": {"title": "Engineer", "company": "Acme"}
            }),
        }
    }

    // ── decide_ingest ────────────────────────────────────────────

    #[test]
    fn new_lead_emits_ingested() {
        let (kind, events) = decide_ingest(
            &LeadState::default(),
            &identity(),
            "drop-in",
            "unknown",
            Some("https://example.com/j".into()),
            "body".into(),
            extracted("Engineer"),
            vec![],
            Some(score_result()),
        )
        .unwrap();
        assert_eq!(kind, IngestKind::New);
        assert_eq!(events.len(), 2);
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
            "unknown",
            Some("https://example.com/j".into()),
            "body".into(),
            extracted("Senior Engineer"),
            vec![],
            Some(score_result()),
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
    fn gate_failures_emit_rejected_with_revision() {
        let failure = GateFailure {
            gate: crate::domain::gates::Gate::RemoteOnly,
            reason: "confident non-remote".into(),
        };
        let (kind, events) = decide_ingest(
            &LeadState::default(),
            &identity(),
            "drop-in",
            "unknown",
            Some("https://example.com/j".into()),
            "body".into(),
            extracted("Engineer"),
            vec![failure],
            None,
        )
        .unwrap();
        assert_eq!(kind, IngestKind::New);
        assert_eq!(events.len(), 2);
        assert_eq!(events[1].event_type, event_type::REJECTED);
        assert_eq!(events[1].payload["gate"], "remote-only");
        assert_eq!(events[1].payload["revision"], 1);
    }

    #[test]
    fn revision_increments_across_evaluations() {
        // A lead previously evaluated once (one rejected event) re-evaluates
        // at revision 2.
        let state = LeadState {
            exists: true,
            seq: 2,
            eval_revision: 1,
            ..Default::default()
        };
        let failure = GateFailure {
            gate: crate::domain::gates::Gate::Blacklist,
            reason: "blacklisted".into(),
        };
        let (_, events) = decide_ingest(
            &state,
            &identity(),
            "drop-in",
            "unknown",
            Some("https://example.com/j".into()),
            "body2".into(),
            extracted("Engineer"),
            vec![failure],
            None,
        )
        .unwrap();
        assert_eq!(events[1].payload["revision"], 2);
    }

    #[test]
    fn scored_revision_increments_across_evaluations() {
        // Scored analog of revision_increments_across_evaluations: a lead
        // previously evaluated once scores at revision 2 on re-evaluation.
        let state = LeadState {
            exists: true,
            seq: 2,
            eval_revision: 1,
            ..Default::default()
        };
        let (_, events) = decide_ingest(
            &state,
            &identity(),
            "drop-in",
            "unknown",
            Some("https://example.com/j".into()),
            "body2".into(),
            extracted("Engineer"),
            vec![],
            Some(score_result()),
        )
        .unwrap();
        let scored = events.last().unwrap();
        assert_eq!(scored.event_type, event_type::SCORED);
        assert_eq!(scored.payload["revision"], 2);
    }

    #[test]
    fn evaluation_cannot_be_both_rejected_and_scored() {
        // Pass-marker invariant (decision 0006): one evaluation emits
        // `rejected` XOR `scored`. Today decide_ingest happily emits both —
        // the invariant is a call-site convention only. (The suppressed
        // path's empty-failures + no-score combination stays legal.)
        let failure = GateFailure {
            gate: crate::domain::gates::Gate::RemoteOnly,
            reason: "on-site".into(),
        };
        let result = decide_ingest(
            &LeadState::default(),
            &identity(),
            "drop-in",
            "unknown",
            Some("https://example.com/j".into()),
            "body".into(),
            extracted("Engineer"),
            vec![failure],
            Some(score_result()),
        );
        assert!(result.is_err());
    }

    #[test]
    fn passing_evaluation_without_a_score_is_rejected() {
        // Pass-marker invariant, second mode (decision 0006): a gate-passing
        // evaluation must emit `scored`. Empty failures + no score would
        // append a markerless snapshot — a lead indistinguishable from the
        // torn batch the pass-marker exists to reject. (The suppressed path
        // stays legal: it returns before evaluation events are considered.)
        let result = decide_ingest(
            &LeadState::default(),
            &identity(),
            "drop-in",
            "unknown",
            Some("https://example.com/j".into()),
            "body".into(),
            extracted("Engineer"),
            vec![],
            None,
        );
        assert!(result.is_err());
    }

    #[test]
    fn multi_failure_evaluation_stays_one_revision() {
        // Regression: one evaluation failing two gates emits two `rejected`
        // events at the SAME revision, and the next evaluation is revision
        // 2 — counting events instead of reading the payload revision would
        // over-count.
        let failures = vec![
            GateFailure {
                gate: crate::domain::gates::Gate::RemoteOnly,
                reason: "on-site".into(),
            },
            GateFailure {
                gate: crate::domain::gates::Gate::Blacklist,
                reason: "blacklisted".into(),
            },
        ];
        let (_, events) = decide_ingest(
            &LeadState::default(),
            &identity(),
            "drop-in",
            "unknown",
            Some("https://example.com/j".into()),
            "body".into(),
            extracted("Engineer"),
            failures,
            None,
        )
        .unwrap();
        assert_eq!(events.len(), 3);
        assert_eq!(events[1].payload["revision"], 1);
        assert_eq!(events[2].payload["revision"], 1);

        // Replay both rejections through evolve, then re-evaluate.
        let mut state = LeadState::default();
        for event_type_and_revision in [("ingested", 0), ("rejected", 1), ("rejected", 1)] {
            let (t, revision) = event_type_and_revision;
            let payload = if t == "ingested" {
                serde_json::json!({
                    "dedupe_key": "url:https://example.com/j",
                    "identifiers": {"url": "url:https://example.com/j"},
                    "adapter": "drop-in",
                    "raw_text": "body",
                    "extracted": {"title": "Engineer", "company": "Acme"}
                })
            } else {
                serde_json::json!({"gate": "remote-only", "reason": "x", "revision": revision})
            };
            state.seq += 1;
            let seq = state.seq;
            evolve(
                &mut state,
                &EventEnvelope {
                    envelope_version: 1,
                    id: Uuid::now_v7(),
                    stream: "lead/x".into(),
                    seq,
                    event_type: t.into(),
                    schema_version: 1,
                    occurred_at: jiff::Timestamp::now(),
                    recorded_at: jiff::Timestamp::now(),
                    causation_id: None,
                    correlation_id: Uuid::now_v7(),
                    payload,
                },
            );
        }
        assert_eq!(state.eval_revision, 1);

        let failure = GateFailure {
            gate: crate::domain::gates::Gate::RemoteOnly,
            reason: "still on-site".into(),
        };
        let (_, events) = decide_ingest(
            &state,
            &identity(),
            "drop-in",
            "unknown",
            Some("https://example.com/j".into()),
            "body".into(),
            extracted("Engineer"),
            vec![failure],
            None,
        )
        .unwrap();
        assert_eq!(events[1].payload["revision"], 2);
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
            "unknown",
            Some("https://example.com/j".into()),
            "body".into(),
            extracted("Engineer"),
            vec![],
            None,
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
            "unknown",
            Some("https://example.com/j".into()),
            "body".into(),
            extracted("Engineer"),
            vec![],
            Some(score_result()),
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
            "unknown",
            Some("https://example.com/new".into()),
            "body".into(),
            extracted("Engineer"),
            vec![],
            Some(score_result()),
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
                "adapter": "drop-in",
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
    fn evolve_scored_sets_latest_score() {
        let mut state = LeadState::default();
        let ingested = ingested_envelope();
        evolve(&mut state, &ingested);

        let scored = EventEnvelope {
            event_type: event_type::SCORED.into(),
            seq: 2,
            payload: serde_json::json!({
                "composite": 75,
                "revision": 1,
                "dimensions": [],
                "breakdown": "75"
            }),
            ..ingested
        };
        evolve(&mut state, &scored);

        let score = state.latest_score.as_ref().unwrap();
        assert_eq!(score.composite, 75);
        assert_eq!(score.revision, 1);
    }

    #[test]
    fn evolve_snapshot_invalidates_stale_score() {
        // Decision 0006: `scored` is the pass-marker. A snapshot without its
        // scored event (torn batch) must not keep an old score current.
        let mut state = LeadState::default();
        let ingested = ingested_envelope();
        evolve(&mut state, &ingested);

        let scored = EventEnvelope {
            event_type: event_type::SCORED.into(),
            seq: 2,
            payload: serde_json::json!({
                "composite": 75,
                "revision": 1,
                "dimensions": [],
                "breakdown": "75"
            }),
            ..ingested.clone()
        };
        evolve(&mut state, &scored);

        let updated = EventEnvelope {
            event_type: event_type::UPDATED.into(),
            seq: 3,
            payload: serde_json::json!({
                "dedupe_key": "tc:abc",
                "identifiers": {"tc": "tc:abc"},
                "adapter": "drop-in",
                "raw_text": "body v2",
                "extracted": {"title": "Senior Engineer", "company": "Acme"}
            }),
            ..ingested
        };
        evolve(&mut state, &updated);

        assert!(state.latest_score.is_none());
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
                "adapter": "drop-in",
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
            "unknown",
            Some("https://example.com/j".into()),
            "body".into(),
            extracted("Engineer"),
            vec![],
            None,
        )
        .unwrap();
        assert_eq!(kind, IngestKind::Suppressed);
        assert_eq!(events[0].event_type, event_type::REINGEST_SUPPRESSED);
    }

    // ── decide_mark ──────────────────────────────────────────────

    fn apply_payload() -> ApplyQueuedPayload {
        ApplyQueuedPayload {
            package: crate::domain::events::ApplyPackage {
                cover_letter_path: Some("/tmp/letter.pdf".into()),
                resume_path: Some("/tmp/resume.pdf".into()),
                cheat_sheet: vec![crate::domain::events::CheatSheetEntry {
                    question: "Full name".into(),
                    answer: "Grey".into(),
                }],
            },
            url: Some("https://example.com/j".into()),
        }
    }

    #[test]
    fn mark_emits_reviewed() {
        let events = decide_mark("defer", None, None).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, event_type::REVIEWED);
        assert_eq!(events[0].payload["mark"], "defer");
    }

    #[test]
    fn apply_automatically_emits_reviewed_and_apply_queued() {
        let events = decide_mark("apply-automatically", None, Some(apply_payload())).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event_type, event_type::REVIEWED);
        assert_eq!(events[1].event_type, event_type::APPLY_QUEUED);
        assert_eq!(events[1].payload["url"], "https://example.com/j");
    }

    #[test]
    fn unknown_mark_is_rejected() {
        assert!(decide_mark("bogus", None, None).is_err());
    }

    #[test]
    fn apply_automatically_requires_a_package() {
        assert!(decide_mark("apply-automatically", None, None).is_err());
    }

    #[test]
    fn non_apply_marks_reject_a_package() {
        assert!(decide_mark("defer", None, Some(apply_payload())).is_err());
    }
}
