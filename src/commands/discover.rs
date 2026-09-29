//! `gwl-jobs discover`: run the discovery layer (OpenSpec change
//! `discovery-ingestion`) — fetch postings from feed sources and ingest them
//! through the existing pipeline.

use std::{collections::HashMap, io::IsTerminal};

use miette::{IntoDiagnostic, Result, miette};
use serde::Serialize;
use tracing::{info, instrument};
use uuid::Uuid;

use super::{open_workspace, record_ingest_correlated};
use crate::{
    cli::DiscoverArgs,
    config::{AppPaths, Config},
    discovery,
    domain::events::{DiscoveryPayload, PendingEvent, event_type},
    event_store::{EventStore, MemStore, read_only_envelopes},
    ingest::{self, IngestOutcome},
    projections, resume,
};

/// The result of a discovery run: per-posting outcome counts plus sources
/// whose fetch failed entirely.
#[derive(Clone, Debug, Default, Serialize)]
pub struct BatchSummary {
    pub new: u64,
    pub updated: u64,
    pub suppressed: u64,
    pub rejected: u64,
    pub failed: u64,
    pub failed_sources: Vec<String>,
    /// Credit exhaustion: results a paid source could not return.
    pub truncated_results: u64,
    /// Fetched records with no workplace-type signal.
    pub unknown_workplace: u64,
    /// Fetched records with no salary signal.
    pub unknown_salary: u64,
    /// Fetched records strict mode would have dropped (unknown workplace or
    /// salary).
    pub strict_would_drop: u64,
    /// True when this summary is a `--dry-run` preview (no events written),
    /// so a scripted `--json` consumer can tell a preview from a real run.
    pub dry_run: bool,
}

/// Decision for gating a paid `--dry-run`: proceed, prompt, or refuse.
enum DryRunGate {
    Proceed,
    Prompt,
    Refuse { reason: String },
}

/// The paid `--dry-run` gate (design decision 7): a dry-run performs the
/// real fetch and skips only the writes, so it spends credits. Before
/// fetching a source that charges per record, the driver asks — unless `--yes`
/// skips the prompt, or the session cannot prompt (`--json` / non-TTY), in
/// which case it refuses rather than corrupt the output or hang.
fn dry_run_gate(
    dry_run: bool,
    yes: bool,
    json: bool,
    stdin_tty: bool,
    paid_sources: &[&str],
) -> DryRunGate {
    if !dry_run || paid_sources.is_empty() {
        return DryRunGate::Proceed;
    }
    let names = paid_sources.join(", ");
    if json {
        return DryRunGate::Refuse {
            reason: format!(
                "--dry-run with a paid source ({names}) would spend credits; \
                 a prompt would corrupt --json output — pass --yes to proceed"
            ),
        };
    }
    if yes {
        return DryRunGate::Proceed;
    }
    if !stdin_tty {
        return DryRunGate::Refuse {
            reason: format!(
                "--dry-run with a paid source ({names}) would spend credits; \
                 refusing to prompt in a non-interactive session — pass --yes to proceed"
            ),
        };
    }
    DryRunGate::Prompt
}

/// Derive each source's prior `discovered_at` watermark from the most recent
/// `discovery` run event (read-only; no writer lock).
fn prior_watermark(paths: &AppPaths) -> Result<HashMap<String, String>> {
    let events = read_only_envelopes(paths.event_log())?;
    for event in events.iter().rev() {
        if event.event_type == event_type::DISCOVERY {
            let payload: DiscoveryPayload =
                serde_json::from_value(event.payload.clone()).into_diagnostic()?;
            return Ok(payload.discovered_at.unwrap_or_default());
        }
    }
    Ok(HashMap::new())
}

#[instrument(skip_all)]
pub async fn execute_discover(
    args: DiscoverArgs,
    config: &Config,
    paths: &AppPaths,
    json: bool,
) -> Result<()> {
    // Load the resume before any network I/O (decision 0004), as ingest does.
    let resume = resume::load(config.resume_path.as_deref())?;
    let resume_skills = resume
        .as_ref()
        .map(resume::Resume::keywords)
        .unwrap_or_default();

    let client = ingest::default_client()?;
    let run_id = Uuid::now_v7();
    // Derive prior watermarks BEFORE fetching: a paid source's query needs
    // its last `discovered_at` as the `discovered_at_gte` lower bound.
    let prior = prior_watermark(paths)?;
    let sources = discovery::build_sources(config, args.source.as_deref(), &prior)?;

    if sources.is_empty() {
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(&BatchSummary::default()).into_diagnostic()?
            );
        } else {
            println!("no enabled sources (nothing to discover)");
        }
        return Ok(());
    }

    // Gate a paid --dry-run BEFORE any credits are spent.
    let paid: Vec<&str> = sources
        .iter()
        .filter(|(_, s)| s.charges_per_record())
        .map(|(name, _)| name.as_str())
        .collect();
    match dry_run_gate(
        args.dry_run,
        args.yes,
        json,
        std::io::stdin().is_terminal(),
        &paid,
    ) {
        DryRunGate::Proceed => {}
        DryRunGate::Refuse { reason } => return Err(miette!(reason)),
        DryRunGate::Prompt => {
            eprint!(
                "will spend credits for {} (1 per record) — proceed? [y/N] ",
                paid.join(", ")
            );
            let mut answer = String::new();
            std::io::stdin().read_line(&mut answer).into_diagnostic()?;
            if !(answer.trim().eq_ignore_ascii_case("y")
                || answer.trim().eq_ignore_ascii_case("yes"))
            {
                eprintln!("aborted");
                return Ok(());
            }
        }
    }

    // Fetch sources (no writer lock); a source failure is non-fatal.
    let fetches = discovery::fetch_sources(&sources, &client).await;

    // Flatten into (source, outcome) pairs, collecting failed sources,
    // per-posting failures, truncation, and the new per-source watermarks.
    let mut outcomes: Vec<(String, IngestOutcome)> = Vec::new();
    let mut failed_sources: Vec<String> = Vec::new();
    let mut failed_postings = 0u64;
    let mut truncated_results = 0u64;
    let mut new_watermark: HashMap<String, String> = HashMap::new();
    for fetch in fetches {
        match fetch.batch {
            Ok(batch) => {
                for o in batch.outcomes {
                    outcomes.push((fetch.source.clone(), o));
                }
                failed_postings += batch.failed;
                truncated_results += batch.truncated_results;
                if let Some(ts) = batch.discovered_at {
                    new_watermark.insert(fetch.source, ts);
                }
            }
            Err(_) => failed_sources.push(fetch.source),
        }
    }

    let rates = discovery::null_rates(outcomes.iter().map(|(_, o)| o));

    if args.dry_run {
        // Dry run: read the corpus WITHOUT the writer lock, seed an
        // in-memory store, and run the same decide → append loop against it.
        // Nothing is written; the summary is what a real run would do.
        let events = read_only_envelopes(paths.event_log())?;
        let mut store = MemStore::seeded(events);
        let mut summary =
            ingest_outcomes_correlated(&mut store, config, &resume_skills, outcomes, run_id)?;
        summary.failed = failed_postings;
        summary.failed_sources = failed_sources;
        summary.truncated_results = truncated_results;
        summary.unknown_workplace = rates.unknown_workplace;
        summary.unknown_salary = rates.unknown_salary;
        summary.strict_would_drop = rates.strict_would_drop;
        summary.dry_run = true;
        print_summary(&summary, json)?;
        info!(
            new = summary.new,
            updated = summary.updated,
            suppressed = summary.suppressed,
            rejected = summary.rejected,
            failed = summary.failed,
            "discovery dry run complete (no events written)"
        );
        return Ok(());
    }

    // Acquire the single-writer lock only for the decide → append cycle.
    let (mut store, _projection) = open_workspace(paths)?;
    let mut summary =
        ingest_outcomes_correlated(&mut store, config, &resume_skills, outcomes, run_id)?;
    summary.failed = failed_postings;
    summary.failed_sources = failed_sources;
    summary.truncated_results = truncated_results;
    summary.unknown_workplace = rates.unknown_workplace;
    summary.unknown_salary = rates.unknown_salary;
    summary.strict_would_drop = rates.strict_would_drop;
    append_discovery_event(&mut store, run_id, &summary, new_watermark)?;
    print_summary(&summary, json)?;
    info!(
        new = summary.new,
        updated = summary.updated,
        suppressed = summary.suppressed,
        rejected = summary.rejected,
        failed = summary.failed,
        "discovery complete"
    );
    Ok(())
}

/// Render a run summary: the counts plus, on the human path, a dry-run or
/// real-run header.
fn print_summary(summary: &BatchSummary, json: bool) -> Result<()> {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(summary).into_diagnostic()?
        );
        return Ok(());
    }
    println!(
        "{}",
        if summary.dry_run {
            "dry run complete — no events written"
        } else {
            "discovery complete"
        }
    );
    println!("  new:        {}", summary.new);
    println!("  updated:    {}", summary.updated);
    println!("  suppressed: {}", summary.suppressed);
    println!("  rejected:   {}", summary.rejected);
    println!("  failed:     {}", summary.failed);
    println!("  truncated:  {}", summary.truncated_results);
    if summary.unknown_workplace > 0 || summary.unknown_salary > 0 || summary.strict_would_drop > 0
    {
        println!("  unknown workplace: {}", summary.unknown_workplace);
        println!("  unknown salary:    {}", summary.unknown_salary);
        println!("  strict would drop: {}", summary.strict_would_drop);
    }
    if !summary.failed_sources.is_empty() {
        println!("  failed sources: {}", summary.failed_sources.join(", "));
    }
    Ok(())
}

/// The decide → append core of a discovery run (under the writer lock): for
/// each extracted outcome, re-project the read model and run the existing
/// `record_ingest`, re-projecting between postings so within-batch duplicates
/// never mint a second lead (design: re-project per posting).
pub fn ingest_outcomes(
    store: &mut impl EventStore,
    config: &Config,
    resume_skills: &[String],
    outcomes: Vec<(String, IngestOutcome)>,
) -> Result<BatchSummary> {
    ingest_outcomes_correlated(store, config, resume_skills, outcomes, Uuid::now_v7())
}

/// `ingest_outcomes` with a caller-supplied run id: a discovery run shares
/// one correlation id across all its posting events (and the trailing
/// `discovery` event), so the whole run is one queryable unit in the log.
fn ingest_outcomes_correlated(
    store: &mut impl EventStore,
    config: &Config,
    resume_skills: &[String],
    outcomes: Vec<(String, IngestOutcome)>,
    run_id: Uuid,
) -> Result<BatchSummary> {
    let mut summary = BatchSummary::default();
    for (source, mut outcome) in outcomes {
        outcome.source = source;
        // Re-project: the read model is always `rebuild(log)`, and the prior
        // posting's appends must be visible before this one's identity lookup.
        let events = store.replay()?;
        let projection = projections::rebuild(&events)?;
        let ingested =
            record_ingest_correlated(store, &projection, config, resume_skills, outcome, run_id)?;
        if ingested.rejected.is_some() {
            summary.rejected += 1;
        } else {
            match ingested.kind {
                "ingested" => summary.new += 1,
                "updated" => summary.updated += 1,
                "reingest_suppressed" => summary.suppressed += 1,
                other => return Err(miette!("unexpected ingest kind '{other}'")),
            }
        }
    }
    Ok(summary)
}

/// Append the run's `discovery` event: the durable record of the run's
/// outcome, on a non-lead `discovery/<run_id>` stream (ignored by the lead
/// projection), sharing the run's correlation id. `discovered_at` records the
/// per-source watermark for the next run's cursor.
fn append_discovery_event(
    store: &mut impl EventStore,
    run_id: Uuid,
    summary: &BatchSummary,
    discovered_at: HashMap<String, String>,
) -> Result<()> {
    let payload = DiscoveryPayload {
        new: summary.new,
        updated: summary.updated,
        suppressed: summary.suppressed,
        rejected: summary.rejected,
        failed: summary.failed,
        failed_sources: summary.failed_sources.clone(),
        discovered_at: if discovered_at.is_empty() {
            None
        } else {
            Some(discovered_at)
        },
    };
    let pending = PendingEvent::new(event_type::DISCOVERY, None, &payload)?;
    store.append(&format!("discovery/{run_id}"), 0, &[pending], run_id)?;
    info!(run_id = %run_id, "discovery run recorded");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        cli::Mark,
        commands::{mark_lead, test_support::*},
        config::{AppPaths, Config},
        event_store::{JsonlEventStore, MemStore, read_only_envelopes},
        ingest::IngestOutcome,
        projections,
    };

    fn outcome_with_url(url: &str) -> IngestOutcome {
        let mut o = outcome("body");
        o.url = Some(url.into());
        o
    }

    // ── ingest_outcomes: dedupe → summary → idempotence → ignore ──

    #[test]
    fn two_postings_same_job_produce_one_lead() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = JsonlEventStore::open(dir.path().join("events.jsonl")).unwrap();
        let config = Config::default();
        let outcomes = vec![
            (
                "remotive".to_string(),
                outcome_with_url("https://example.com/job/1"),
            ),
            (
                "wwr".to_string(),
                outcome_with_url("https://example.com/job/1"),
            ),
        ];

        let summary = ingest_outcomes(&mut store, &config, &[], outcomes).unwrap();

        assert_eq!(summary.new, 1);
        assert_eq!(summary.updated, 1);
        let projection = projections::rebuild(&store.replay().unwrap()).unwrap();
        assert_eq!(projection.leads.len(), 1);
    }

    #[test]
    fn summary_counts_new_updated_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = JsonlEventStore::open(dir.path().join("events.jsonl")).unwrap();
        let config = Config {
            remote_only: true,
            ..Default::default()
        };

        let mut passing = outcome_with_url("https://example.com/a");
        passing.extracted.remote = Some(true);
        let mut rejected = outcome_with_url("https://example.com/b");
        rejected.extracted.remote = Some(false);

        let summary = ingest_outcomes(
            &mut store,
            &config,
            &[],
            vec![("remotive".into(), passing), ("remotive".into(), rejected)],
        )
        .unwrap();
        assert_eq!(summary.new, 1);
        assert_eq!(summary.rejected, 1);

        // Re-ingest the passing lead → updated.
        let mut again = outcome_with_url("https://example.com/a");
        again.extracted.remote = Some(true);
        let summary =
            ingest_outcomes(&mut store, &config, &[], vec![("remotive".into(), again)]).unwrap();
        assert_eq!(summary.updated, 1);
    }

    #[test]
    fn re_running_discovery_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = JsonlEventStore::open(dir.path().join("events.jsonl")).unwrap();
        let config = Config::default();
        let mut passing = outcome_with_url("https://example.com/a");
        passing.extracted.remote = Some(true);

        let first = ingest_outcomes(
            &mut store,
            &config,
            &[],
            vec![("remotive".into(), passing.clone())],
        )
        .unwrap();
        assert_eq!(first.new, 1);

        let second =
            ingest_outcomes(&mut store, &config, &[], vec![("remotive".into(), passing)]).unwrap();
        assert_eq!(second.new, 0);
        assert_eq!(second.updated, 1);
        let projection = projections::rebuild(&store.replay().unwrap()).unwrap();
        assert_eq!(projection.leads.len(), 1);
    }

    #[test]
    fn ignored_lead_is_suppressed_on_rediscovery() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = JsonlEventStore::open(dir.path().join("events.jsonl")).unwrap();
        let config = Config::default();
        let mut passing = outcome_with_url("https://example.com/a");
        passing.extracted.remote = Some(true);

        let first =
            ingest_outcomes(&mut store, &config, &[], vec![("remotive".into(), passing)]).unwrap();
        assert_eq!(first.new, 1);

        // Mark the lead ignore.
        let projection = projections::rebuild(&store.replay().unwrap()).unwrap();
        let record = projection.leads.values().next().unwrap();
        mark_lead(&mut store, &config, record, Mark::Ignore, None).unwrap();

        // Rediscovery → suppressed.
        let mut again = outcome_with_url("https://example.com/a");
        again.extracted.remote = Some(true);
        let summary =
            ingest_outcomes(&mut store, &config, &[], vec![("remotive".into(), again)]).unwrap();
        assert_eq!(summary.suppressed, 1);
        assert_eq!(summary.new, 0);
    }

    #[test]
    fn dry_run_core_reports_without_writing_the_real_log() {
        // The dry-run path: read the corpus read-only, seed an in-memory
        // store, run the decide loop against it — and leave the real log
        // untouched (one original lead's ingested + scored events).
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("events.jsonl");
        {
            let mut store = JsonlEventStore::open(&log_path).unwrap();
            let mut existing = outcome_with_url("https://example.com/existing");
            existing.extracted.remote = Some(true);
            ingest_outcomes(
                &mut store,
                &Config::default(),
                &[],
                vec![("remotive".into(), existing)],
            )
            .unwrap();
        }

        let events = read_only_envelopes(&log_path).unwrap();
        let mut mem = MemStore::seeded(events);

        let mut new_posting = outcome_with_url("https://example.com/new");
        new_posting.extracted.remote = Some(true);
        let mut duplicate = outcome_with_url("https://example.com/existing");
        duplicate.extracted.remote = Some(true);

        let summary = ingest_outcomes_correlated(
            &mut mem,
            &Config::default(),
            &[],
            vec![
                ("remotive".into(), new_posting),
                ("remotive".into(), duplicate),
            ],
            Uuid::now_v7(),
        )
        .unwrap();

        // One new lead, one re-ingest of the existing lead.
        assert_eq!(summary.new, 1);
        assert_eq!(summary.updated, 1);
        assert_eq!(summary.suppressed, 0);
        assert_eq!(summary.rejected, 0);

        // The real log is unchanged: still just the original lead's events.
        assert_eq!(read_only_envelopes(&log_path).unwrap().len(), 2);
    }

    #[test]
    fn summary_json_marks_dry_runs() {
        // A scripted --json consumer must be able to tell a preview from a
        // real run: the dry_run marker is part of the machine contract.
        let preview = BatchSummary {
            dry_run: true,
            ..Default::default()
        };
        let value = serde_json::to_value(&preview).unwrap();
        assert_eq!(value["dry_run"], true);

        let real = BatchSummary::default();
        assert_eq!(serde_json::to_value(&real).unwrap()["dry_run"], false);
    }

    // ── discovery run event ────────────────────────────────────

    #[test]
    fn run_appends_discovery_event_with_shared_correlation_id() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = JsonlEventStore::open(dir.path().join("events.jsonl")).unwrap();
        let config = Config::default();
        let run_id = Uuid::now_v7();

        let mut a = outcome_with_url("https://example.com/a");
        a.extracted.remote = Some(true);
        let mut b = outcome_with_url("https://example.com/b");
        b.extracted.remote = Some(true);

        let mut summary = ingest_outcomes_correlated(
            &mut store,
            &config,
            &[],
            vec![("remotive".into(), a), ("remotive".into(), b)],
            run_id,
        )
        .unwrap();
        summary.failed = 1;
        summary.failed_sources = vec!["wwr".into()];
        append_discovery_event(&mut store, run_id, &summary, HashMap::new()).unwrap();

        let events = store.replay().unwrap();
        // 2 leads × (ingested + scored) = 4, plus the discovery event = 5.
        assert_eq!(events.len(), 5);
        // Every event in the run shares the run's correlation id.
        for e in &events {
            assert_eq!(e.correlation_id, run_id);
        }
        let discovery = events
            .iter()
            .find(|e| e.event_type == event_type::DISCOVERY)
            .expect("a discovery event");
        assert_eq!(discovery.stream, format!("discovery/{run_id}"));
        let payload: DiscoveryPayload = serde_json::from_value(discovery.payload.clone()).unwrap();
        assert_eq!(payload.new, 2);
        assert_eq!(payload.failed, 1);
        assert_eq!(payload.failed_sources, vec!["wwr".to_string()]);
    }

    // ── watermark cursor (theirstack-paid-feed increment 3) ─────

    #[test]
    fn discovery_event_roundtrips_watermark() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = JsonlEventStore::open(dir.path().join("events.jsonl")).unwrap();
        let run_id = Uuid::now_v7();
        let summary = BatchSummary {
            new: 1,
            ..Default::default()
        };
        let mut watermark = HashMap::new();
        watermark.insert("theirstack".to_string(), "2024-01-01T00:00:00Z".to_string());
        append_discovery_event(&mut store, run_id, &summary, watermark).unwrap();

        let events = store.replay().unwrap();
        let discovery = events
            .iter()
            .find(|e| e.event_type == event_type::DISCOVERY)
            .unwrap();
        let payload: DiscoveryPayload = serde_json::from_value(discovery.payload.clone()).unwrap();
        assert_eq!(
            payload
                .discovered_at
                .as_ref()
                .and_then(|m| m.get("theirstack"))
                .map(String::as_str),
            Some("2024-01-01T00:00:00Z")
        );
    }

    #[test]
    fn prior_watermark_derives_from_last_discovery_event() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().join("data");
        std::fs::create_dir_all(&data_dir).unwrap();
        let paths = AppPaths::new(dir.path().join("config"), data_dir);

        {
            let mut store = JsonlEventStore::open(paths.event_log()).unwrap();
            let mut watermark = HashMap::new();
            watermark.insert("theirstack".to_string(), "2024-01-02T00:00:00Z".to_string());
            append_discovery_event(
                &mut store,
                Uuid::now_v7(),
                &BatchSummary::default(),
                watermark,
            )
            .unwrap();
        }

        let prior = prior_watermark(&paths).unwrap();
        assert_eq!(
            prior.get("theirstack").map(String::as_str),
            Some("2024-01-02T00:00:00Z")
        );
    }

    // ── paid --dry-run gate (increment 2, task 2.8) ─────────────

    #[test]
    fn dry_run_gate_paths() {
        let paid = ["theirstack"];
        // A real run spends credits by definition; no gate.
        assert!(matches!(
            dry_run_gate(false, false, false, false, &paid),
            DryRunGate::Proceed
        ));
        // No paid sources: nothing to gate.
        assert!(matches!(
            dry_run_gate(true, false, false, true, &[]),
            DryRunGate::Proceed
        ));
        // --json refuses rather than prompt (a prompt corrupts the JSON).
        assert!(matches!(
            dry_run_gate(true, false, true, true, &paid),
            DryRunGate::Refuse { .. }
        ));
        // --yes skips the prompt, even in a non-TTY session.
        assert!(matches!(
            dry_run_gate(true, true, false, false, &paid),
            DryRunGate::Proceed
        ));
        // TTY without --yes: prompt.
        assert!(matches!(
            dry_run_gate(true, false, false, true, &paid),
            DryRunGate::Prompt
        ));
        // Non-TTY without --yes: refuse rather than hang.
        assert!(matches!(
            dry_run_gate(true, false, false, false, &paid),
            DryRunGate::Refuse { .. }
        ));
    }

    // ── summary null-rate / truncation fields (tasks 2.7, 3.3) ──

    #[test]
    fn summary_serializes_null_rate_and_truncation_fields() {
        let summary = BatchSummary {
            truncated_results: 7,
            unknown_workplace: 1,
            unknown_salary: 2,
            strict_would_drop: 3,
            ..Default::default()
        };
        let value = serde_json::to_value(&summary).unwrap();
        assert_eq!(value["truncated_results"], 7);
        assert_eq!(value["unknown_workplace"], 1);
        assert_eq!(value["unknown_salary"], 2);
        assert_eq!(value["strict_would_drop"], 3);
    }
}
