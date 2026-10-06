//! `gwl-jobs rescore` (OpenSpec change rescore-evaluation): re-run gates
//! and scoring with the *current* config over each lead's stored snapshot,
//! appending evaluation events only (`rejected` XOR `scored`, decision
//! 0006). No refetch (`raw_text` is persisted), no snapshot event, and
//! marks/outcomes untouched — a rescore is not a review decision.
//!
//! The dry run evaluates against the same decide → append loop seeded into
//! an in-memory store (the `discover --dry-run` pattern), so the preview
//! cannot drift from a real run.

use miette::{IntoDiagnostic, Result, miette};
use serde::Serialize;
use serde_with::skip_serializing_none;
use tracing::{Span, debug, info, instrument};
use uuid::Uuid;

use super::{open_workspace, replay_lead, select_lead};
use crate::{
    cli::RescoreArgs,
    config::{AppPaths, Config},
    domain::{
        gates::{self, GateFailure},
        lead::{self, LeadState},
        scoring::{self, ScoreResult},
    },
    event_store::{EventStore, MemStore, read_only_envelopes},
    projections::{self, LeadRecord, Projection},
    render, resume,
};

/// What a re-evaluation did to one lead, relative to its prior state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RescoreDelta {
    /// Still gate-passing; the composite moved.
    Changed,
    /// Was passing (or never evaluated); now fails a gate.
    NewlyRejected,
    /// Was gate-rejected; now passes and carries a fresh score.
    Revived,
    /// Same verdict, same composite.
    Unchanged,
}

/// One lead's re-evaluation result (the `--json` row and the per-lead line).
#[skip_serializing_none]
#[derive(Clone, Debug, Serialize)]
pub struct RescoreOutcome {
    pub lead_id: Uuid,
    pub title: Option<String>,
    pub old_composite: Option<u64>,
    pub new_composite: Option<u64>,
    pub delta: RescoreDelta,
    /// The gates the new evaluation failed (present when it failed any).
    pub rejected_gates: Vec<String>,
}

/// The `rescore` run summary; the `--json` output carries the same counts
/// as the human summary line.
#[derive(Clone, Debug, Default, Serialize)]
pub struct RescoreSummary {
    pub evaluated: u64,
    pub changed: u64,
    pub newly_rejected: u64,
    pub revived: u64,
    pub unchanged: u64,
    pub dry_run: bool,
    pub results: Vec<RescoreOutcome>,
}

#[instrument(skip_all, fields(dry_run, evaluated, changed, rejected, revived))]
pub async fn execute_rescore(
    args: RescoreArgs,
    config: &Config,
    paths: &AppPaths,
    json: bool,
    color: bool,
) -> Result<()> {
    // Scoring reads the resume (like ingest/edit); a configured-but-broken
    // resume fails loudly (decision 0004) before anything is appended.
    let resume = resume::load(config.resume_path.as_deref())?;
    let resume_skills = resume
        .as_ref()
        .map(resume::Resume::keywords)
        .unwrap_or_default();

    // One correlation id for the whole run: every evaluation event the run
    // appends is answerable as one operator action.
    let correlation_id = Uuid::now_v7();

    // The dry run reads the corpus WITHOUT the writer lock and appends to an
    // in-memory store that is dropped on exit — the same decide → append
    // loop, so the preview cannot drift from a real run (design decision 4).
    let (summary, record_after) = if args.dry_run {
        let events = read_only_envelopes(paths.event_log())?;
        let mut store = MemStore::seeded(events);
        let projection = projections::rebuild(&store.replay()?)?;
        let summary = run(
            &mut store,
            &projection,
            config,
            &resume_skills,
            &args,
            correlation_id,
        )?;
        (summary, None)
    } else {
        let (mut store, projection) = open_workspace(paths)?;
        let summary = run(
            &mut store,
            &projection,
            config,
            &resume_skills,
            &args,
            correlation_id,
        )?;
        let record_after = summary
            .results
            .first()
            .filter(|_| summary.evaluated == 1)
            .map(|r| r.lead_id);
        (summary, record_after)
    };

    // Span fields (decision 0011): the counts answer "did this work, and
    // how much changed?" at a glance.
    let span = Span::current();
    span.record("dry_run", summary.dry_run);
    span.record("evaluated", summary.evaluated);
    span.record("changed", summary.changed);
    span.record("rejected", summary.newly_rejected);
    span.record("revived", summary.revived);

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&summary).into_diagnostic()?
        );
    } else if summary.evaluated == 1 {
        // Single-lead (non-dry) run: re-render the card so the new score and
        // breakdown are visible immediately (same UX as edit). The dry run
        // has no post-append state to render and prints the delta line only.
        let result = &summary.results[0];
        println!("{}", outcome_line(result));
        if let (Some(lead_id), false) = (record_after, args.dry_run) {
            let (_, projection) = open_workspace(paths)?;
            let record = projection
                .leads
                .get(&lead_id)
                .ok_or_else(|| miette!("lead {lead_id} not found after rescore"))?;
            render::render_card(record, color)?;
        }
    } else {
        for result in &summary.results {
            println!("{}", outcome_line(result));
        }
        println!();
        println!("{}", summary_line(&summary));
    }

    // The info summary mirrors the command output (decision 0011).
    info!(
        evaluated = summary.evaluated,
        changed = summary.changed,
        newly_rejected = summary.newly_rejected,
        revived = summary.revived,
        unchanged = summary.unchanged,
        dry_run = summary.dry_run,
        "rescore complete"
    );
    Ok(())
}

/// One rescore invocation over its target set: resolve targets, evaluate
/// each, append, and summarize.
fn run(
    store: &mut impl EventStore,
    projection: &Projection,
    config: &Config,
    resume_skills: &[String],
    args: &RescoreArgs,
    correlation_id: Uuid,
) -> Result<RescoreSummary> {
    let targets: Vec<LeadRecord> = if let Some(prefix) = &args.lead {
        // The single-lead form is explicit operator action on a named lead:
        // any state, including ignored and terminal (mirrors `edit`).
        vec![select_lead(projection, prefix)?.clone()]
    } else {
        // The sweep is the complement of DR 0013's default-view exclusion
        // minus its gate-rejected clause: non-terminal ∧ non-ignored, with
        // gate-rejected leads INCLUDED (the revival case). Not
        // `active_leads()` — after PR #40 that excludes gate-rejected.
        projection
            .ranked_leads()
            .into_iter()
            .filter(|r| !r.is_terminal() && !r.is_buried())
            .cloned()
            .collect()
    };

    let mut summary = RescoreSummary {
        dry_run: args.dry_run,
        ..Default::default()
    };
    for record in targets {
        let outcome = rescore_lead(store, config, resume_skills, &record, correlation_id)?;
        match outcome.delta {
            RescoreDelta::Changed => summary.changed += 1,
            RescoreDelta::NewlyRejected => summary.newly_rejected += 1,
            RescoreDelta::Revived => summary.revived += 1,
            RescoreDelta::Unchanged => summary.unchanged += 1,
        }
        summary.evaluated += 1;
        summary.results.push(outcome);
    }
    Ok(summary)
}

/// The testable core of one lead's rescore: replay the aggregate, evaluate
/// the stored snapshot with the current config, classify the delta, decide
/// the evaluation events, and append. A no-change re-evaluation still
/// appends (design decision 3): the re-evaluation happened, and the log
/// records it.
fn rescore_lead(
    store: &mut impl EventStore,
    config: &Config,
    resume_skills: &[String],
    record: &LeadRecord,
    correlation_id: Uuid,
) -> Result<RescoreOutcome> {
    let lead_id = record.lead_id;
    let state = replay_lead(store, lead_id)?;
    if !state.exists {
        return Err(miette!("lead {lead_id} has no stored snapshot"));
    }
    let extracted = state.snapshot.clone().unwrap_or_default();
    let raw_text = state.raw_text.as_deref().unwrap_or("");

    let (gate_failures, score) = evaluate(config, resume_skills, &extracted, raw_text);
    let delta = classify(record, &gate_failures, &score);
    debug!(
        %lead_id,
        old = record.latest_score.as_ref().map(|s| s.composite),
        new = score.as_ref().map(|s| s.composite),
        delta = ?delta,
        "lead rescored"
    );

    let pending = lead::decide_reevaluate(&state, gate_failures.clone(), score.clone())?;
    let stream = LeadState::stream_id(lead_id);
    store.append(&stream, state.seq, &pending, correlation_id)?;

    Ok(RescoreOutcome {
        lead_id,
        title: record.extracted.title.clone(),
        old_composite: record.latest_score.as_ref().map(|s| s.composite),
        new_composite: score.as_ref().map(|s| s.composite),
        rejected_gates: gate_failures
            .iter()
            .map(|f| f.gate.as_str().into())
            .collect(),
        delta,
    })
}

/// The shared evaluation helper (design decision 4): the exact code both the
/// real run and the dry run use, so the preview cannot drift.
fn evaluate(
    config: &Config,
    resume_skills: &[String],
    extracted: &crate::domain::events::ExtractedFields,
    raw_text: &str,
) -> (Vec<GateFailure>, Option<ScoreResult>) {
    let gate_failures = gates::evaluate(config, extracted, raw_text);
    let score = if gate_failures.is_empty() {
        Some(scoring::score(config, extracted, raw_text, resume_skills))
    } else {
        None
    };
    (gate_failures, score)
}

/// Classify a re-evaluation against the lead's prior projected state.
fn classify(
    record: &LeadRecord,
    gate_failures: &[GateFailure],
    score: &Option<ScoreResult>,
) -> RescoreDelta {
    let was_rejected = record.is_gate_rejected();
    let newly_rejected = !gate_failures.is_empty();
    match (newly_rejected, was_rejected) {
        (true, true) => RescoreDelta::Unchanged, // still rejected
        (true, false) => RescoreDelta::NewlyRejected,
        (false, true) => RescoreDelta::Revived,
        (false, false) => {
            let old = record.latest_score.as_ref().map(|s| s.composite);
            let new = score.as_ref().map(|s| s.composite);
            if old == new {
                RescoreDelta::Unchanged
            } else {
                RescoreDelta::Changed
            }
        }
    }
}

/// One human-readable per-lead line.
fn outcome_line(result: &RescoreOutcome) -> String {
    let title = result.title.as_deref().unwrap_or("Untitled");
    let lead_prefix: String = result.lead_id.to_string().chars().take(8).collect();
    match result.delta {
        RescoreDelta::Changed => format!(
            "  {lead_prefix}  {title}: {} -> {}",
            result
                .old_composite
                .map(|c| c.to_string())
                .unwrap_or_else(|| "-".into()),
            result.new_composite.unwrap_or_default()
        ),
        RescoreDelta::NewlyRejected => format!(
            "  {lead_prefix}  {title}: rejected ({})",
            result.rejected_gates.join(", ")
        ),
        RescoreDelta::Revived => format!(
            "  {lead_prefix}  {title}: revived ({})",
            result.new_composite.unwrap_or_default()
        ),
        RescoreDelta::Unchanged => {
            if result.rejected_gates.is_empty() {
                format!(
                    "  {lead_prefix}  {title}: unchanged ({})",
                    result.new_composite.unwrap_or_default()
                )
            } else {
                format!(
                    "  {lead_prefix}  {title}: still rejected ({})",
                    result.rejected_gates.join(", ")
                )
            }
        }
    }
}

/// The human summary line (mirrored by the `info!` record).
fn summary_line(summary: &RescoreSummary) -> String {
    let verb = if summary.dry_run {
        "would rescore"
    } else {
        "rescored"
    };
    format!(
        "{} {} leads: {} changed, {} newly rejected, {} revived, {} unchanged",
        verb,
        summary.evaluated,
        summary.changed,
        summary.newly_rejected,
        summary.revived,
        summary.unchanged
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{commands::test_support::*, domain::events::ExtractedFields};

    // ── classify ────────────────────────────────────────────────

    #[test]
    fn classify_changed_newly_rejected_revived_unchanged() {
        let mut record = lead_record(Some("https://example.com/j"));
        record.latest_score = Some(crate::domain::events::ScoredPayload {
            composite: 75,
            revision: 1,
            dimensions: vec![],
            breakdown: "75".into(),
        });
        let score_90 = Some(ScoreResult {
            composite: 90,
            dimensions: vec![],
            breakdown: "90".into(),
        });
        let score_75 = Some(ScoreResult {
            composite: 75,
            dimensions: vec![],
            breakdown: "75".into(),
        });

        // Passing → passing, composite moved.
        assert_eq!(classify(&record, &[], &score_90), RescoreDelta::Changed);
        // Passing → passing, same composite.
        assert_eq!(classify(&record, &[], &score_75), RescoreDelta::Unchanged);
        // Passing → newly rejected.
        let failure = GateFailure {
            gate: crate::domain::gates::Gate::RemoteOnly,
            reason: "on-site".into(),
        };
        assert_eq!(
            classify(&record, std::slice::from_ref(&failure), &None),
            RescoreDelta::NewlyRejected
        );

        // Gate-rejected → passing (revived); gate-rejected → still rejected.
        record.latest_score = None;
        record.latest_rejection = Some(crate::projections::GateRejection {
            gate: "remote-only".into(),
            reason: "on-site".into(),
            revision: 1,
        });
        assert_eq!(classify(&record, &[], &score_75), RescoreDelta::Revived);
        assert_eq!(
            classify(&record, &[failure], &None),
            RescoreDelta::Unchanged
        );
    }

    // ── rescore_lead ────────────────────────────────────────────

    #[test]
    fn rescore_lead_appends_evaluation_event_and_reports_delta() {
        let config = Config::default();
        let dir = tempfile::tempdir().unwrap();
        let (mut store, record) = ingested_lead(
            &dir,
            &config,
            ExtractedFields {
                title: Some("Engineer".into()),
                company: Some("Acme".into()),
                ..Default::default()
            },
            "body",
        );
        let outcome = rescore_lead(&mut store, &config, &[], &record, Uuid::now_v7()).unwrap();
        assert_eq!(outcome.lead_id, record.lead_id);
        assert!(
            outcome.old_composite.is_some(),
            "the ingested lead was scored"
        );
        assert_eq!(outcome.old_composite, outcome.new_composite);
        assert_eq!(outcome.delta, RescoreDelta::Unchanged);

        // A no-change re-evaluation still appends (design decision 3).
        let events = store.replay().unwrap();
        let rescored = events.last().unwrap();
        assert_eq!(rescored.event_type, "scored");
        assert_eq!(rescored.payload["revision"], 2);
    }

    #[test]
    fn rescore_lead_newly_rejected_clears_stale_score_in_projection() {
        // The motivating incident's direction: a lead scored under a
        // permissive config re-evaluates into a rejection. The projection's
        // latest-wins fix (task 1.0) makes it leave the default view instead
        // of lingering mis-tagged with a stale score.
        let permissive = Config::default();
        let dir = tempfile::tempdir().unwrap();
        let (mut store, record) = ingested_lead(
            &dir,
            &permissive,
            ExtractedFields {
                title: Some("Engineer".into()),
                company: Some("Acme".into()),
                remote: Some(false),
                ..Default::default()
            },
            "on-site role",
        );
        assert!(
            record.latest_score.is_some(),
            "scored under the permissive config"
        );

        let strict = Config {
            remote_only: true,
            ..Config::default()
        };
        let outcome = rescore_lead(&mut store, &strict, &[], &record, Uuid::now_v7()).unwrap();
        assert_eq!(outcome.delta, RescoreDelta::NewlyRejected);
        assert_eq!(outcome.new_composite, None);
        assert_eq!(outcome.rejected_gates, vec!["remote-only"]);

        let events = store.replay().unwrap();
        let rescored = events.last().unwrap();
        assert_eq!(rescored.event_type, "rejected");
        assert_eq!(rescored.payload["revision"], 2);

        let projection = projections::rebuild(&events).unwrap();
        let record = projection.leads.get(&record.lead_id).unwrap();
        assert!(
            record.latest_score.is_none(),
            "stale score must not survive"
        );
        assert!(record.is_gate_rejected());
        assert_eq!(record.lifecycle_status(), "rejected (gate)");
        let active: Vec<Uuid> = projection
            .active_leads()
            .iter()
            .map(|r| r.lead_id)
            .collect();
        assert!(!active.contains(&record.lead_id));

        // The printed card reflects the NEW evaluation, not the stored one
        // (remi's riding note): the rejection renders, the stale score line
        // must not.
        let md = crate::render::card_markdown(record);
        assert!(md.contains("**Rejected: remote-only**"), "md: {md}");
        assert!(!md.contains("**Score:"), "md: {md}");
    }

    #[test]
    fn rescore_lead_revives_gate_rejected_lead() {
        // The revival direction: a gate-rejected lead + the fixed config
        // re-enters the default view (and the pending queue).
        let mut config = Config {
            remote_only: true,
            ..Config::default()
        };
        let dir = tempfile::tempdir().unwrap();
        let (mut store, record) = ingested_lead(
            &dir,
            &config,
            ExtractedFields {
                title: Some("Engineer".into()),
                company: Some("Acme".into()),
                remote: Some(false),
                ..Default::default()
            },
            "on-site role",
        );
        assert!(record.is_gate_rejected());

        // The operator fixes the config (the gate was wrong).
        config.remote_only = false;
        let outcome = rescore_lead(&mut store, &config, &[], &record, Uuid::now_v7()).unwrap();
        assert_eq!(outcome.delta, RescoreDelta::Revived);
        assert!(outcome.new_composite.is_some());

        let events = store.replay().unwrap();
        let rescored = events.last().unwrap();
        assert_eq!(rescored.event_type, "scored");
        assert_eq!(rescored.payload["revision"], 2);

        let projection = projections::rebuild(&events).unwrap();
        let revived = projection.leads.get(&record.lead_id).unwrap();
        assert!(!revived.is_gate_rejected());
        assert!(revived.latest_rejection.is_none(), "no ghost rejection");
        let active: Vec<uuid::Uuid> = projection
            .active_leads()
            .iter()
            .map(|r| r.lead_id)
            .collect();
        assert!(active.contains(&record.lead_id));
        assert_eq!(projection.pending_queue().len(), 1);
    }

    #[test]
    fn rescore_dry_run_leaves_log_byte_identical_and_matches_real_run() {
        // Design decision 4: the dry run evaluates through the same loop
        // (MemStore), appends nothing, and reports the same summary a real
        // run would.
        let config = Config::default();
        let dir = tempfile::tempdir().unwrap();
        let (store, _record) = ingested_lead(
            &dir,
            &config,
            ExtractedFields {
                title: Some("Engineer".into()),
                company: Some("Acme".into()),
                ..Default::default()
            },
            "body",
        );
        drop(store); // release the single-writer lock before the dry run reads

        let log_path = dir.path().join("events.jsonl");
        let before = std::fs::read(&log_path).unwrap();

        let events = read_only_envelopes(&log_path).unwrap();
        let mut mem = MemStore::seeded(events);
        let projection = projections::rebuild(&mem.replay().unwrap()).unwrap();
        let args = RescoreArgs {
            lead: None,
            all: true,
            dry_run: true,
        };
        let dry = run(&mut mem, &projection, &config, &[], &args, Uuid::now_v7()).unwrap();
        assert!(dry.dry_run);
        assert_eq!(dry.evaluated, 1);
        assert_eq!(dry.unchanged, 1);

        // Byte-identical log (spec: appends nothing, no writer state).
        let after = std::fs::read(&log_path).unwrap();
        assert_eq!(before, after);

        // A real run over the same corpus reports the same classification.
        let (mut store, projection) = (
            crate::event_store::JsonlEventStore::open(&log_path).unwrap(),
            projection,
        );
        let args = RescoreArgs {
            lead: None,
            all: true,
            dry_run: false,
        };
        let real = run(&mut store, &projection, &config, &[], &args, Uuid::now_v7()).unwrap();
        assert_eq!(real.evaluated, 1);
        assert_eq!(real.unchanged, 1);
        assert_eq!(real.results[0].delta, dry.results[0].delta);
    }

    #[test]
    fn rescore_all_scope_excludes_ignored_and_terminal_keeps_gate_rejected() {
        // Task 2.2: mixed corpus — only the non-terminal, non-ignored leads
        // are touched, and gate-rejected leads are IN scope.
        let config = Config {
            remote_only: true,
            ..Config::default()
        };
        let dir = tempfile::tempdir().unwrap();
        let (store, pending) = ingested_lead(
            &dir,
            &config,
            ExtractedFields {
                title: Some("Pending".into()),
                remote: Some(true),
                ..Default::default()
            },
            "remote body",
        );
        drop(store);
        let (store, rejected) = ingested_lead(
            &dir,
            &config,
            ExtractedFields {
                title: Some("Rejected".into()),
                remote: Some(false),
                ..Default::default()
            },
            "on-site body",
        );
        drop(store);
        let (store, ignored) = ingested_lead(
            &dir,
            &config,
            ExtractedFields {
                title: Some("Ignored".into()),
                remote: Some(true),
                ..Default::default()
            },
            "remote body",
        );
        drop(store);
        let (store, terminal) = ingested_lead(
            &dir,
            &config,
            ExtractedFields {
                title: Some("Terminal".into()),
                remote: Some(true),
                ..Default::default()
            },
            "remote body",
        );
        drop(store);

        // Mark one ignored, one terminal (archived outcome). The outcome
        // append mirrors the outcome command's own helper (private there):
        // replay the stream, append one event at the expected seq.
        let mut store =
            crate::event_store::JsonlEventStore::open(dir.path().join("events.jsonl")).unwrap();
        let projection = projections::rebuild(&store.replay().unwrap()).unwrap();
        crate::commands::mark_lead(
            &mut store,
            &config,
            projection.leads.get(&ignored.lead_id).unwrap(),
            crate::cli::Mark::Ignore,
            None,
        )
        .unwrap();
        let state = super::replay_lead(&store, terminal.lead_id).unwrap();
        let archived_event = crate::domain::events::PendingEvent::new(
            crate::domain::events::event_type::ARCHIVED,
            None,
            &crate::domain::events::OutcomePayload::default(),
        )
        .unwrap();
        store
            .append(
                &crate::domain::lead::LeadState::stream_id(terminal.lead_id),
                state.seq,
                &[archived_event],
                Uuid::now_v7(),
            )
            .unwrap();

        let projection = projections::rebuild(&store.replay().unwrap()).unwrap();
        let args = RescoreArgs {
            lead: None,
            all: true,
            dry_run: false,
        };
        let summary = run(&mut store, &projection, &config, &[], &args, Uuid::now_v7()).unwrap();
        let touched: Vec<uuid::Uuid> = summary.results.iter().map(|r| r.lead_id).collect();
        assert_eq!(summary.evaluated, 2, "pending + gate-rejected only");
        assert!(touched.contains(&pending.lead_id));
        assert!(touched.contains(&rejected.lead_id));
        assert!(!touched.contains(&ignored.lead_id));
        assert!(!touched.contains(&terminal.lead_id));
    }

    // ── output helpers ──────────────────────────────────────────

    #[test]
    fn outcome_and_summary_lines_read_clearly() {
        let result = RescoreOutcome {
            lead_id: uuid::Uuid::now_v7(),
            title: Some("Engineer".into()),
            old_composite: Some(75),
            new_composite: Some(90),
            delta: RescoreDelta::Changed,
            rejected_gates: vec![],
        };
        let line = outcome_line(&result);
        let prefix: String = result.lead_id.to_string().chars().take(8).collect();
        assert!(
            line.contains(&format!("{prefix}  Engineer: 75 -> 90")),
            "line: {line}"
        );

        let summary = RescoreSummary {
            evaluated: 3,
            changed: 1,
            newly_rejected: 1,
            revived: 0,
            unchanged: 1,
            dry_run: false,
            results: vec![],
        };
        let line = summary_line(&summary);
        assert_eq!(
            line,
            "rescored 3 leads: 1 changed, 1 newly rejected, 0 revived, 1 unchanged"
        );
    }
}
