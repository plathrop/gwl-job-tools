//! `gwl-jobs discover`: run the discovery layer (OpenSpec change
//! `discovery-ingestion`) — fetch postings from feed sources and ingest them
//! through the existing pipeline.

use std::{
    collections::{BTreeMap, HashMap},
    io::IsTerminal,
};

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
    /// Client-gate rejections, keyed by source (task 2.7): per-source
    /// rejection counts, so an operator can see which source's postings the
    /// gates are dropping (gating runs after the per-source fetch span closes).
    pub rejected_by_source: BTreeMap<String, u64>,
    /// Why each failed source failed (design decision 8): whole-source
    /// failures AND partial fetches, keyed by source — so a run is diagnosable
    /// from its own summary and event without re-running at a higher log
    /// level (the incident this fixes: a 402 mid-pagination whose only trace
    /// was a warn! beneath the default error file-sink level).
    pub failed_source_reasons: BTreeMap<String, String>,
    /// Credits spent per paid source (`theirstack-spend-control`): records
    /// returned = credits, per TheirStack's pricing. Free sources report
    /// nothing here. The operator's answer to "what did this run cost".
    pub credits_spent_by_source: BTreeMap<String, u64>,
    /// True when this summary is a `--dry-run` preview (no events written),
    /// so a scripted `--json` consumer can tell a preview from a real run.
    pub dry_run: bool,
}

/// A discovery fetch pass, flattened: outcomes ready for ingest plus the
/// per-run facts the summary and the `discovery` event carry. Extracted from
/// `execute_discover` so the salvage semantics (partial batches count as
/// failed sources WITH their records ingested) are unit-testable without a
/// network.
#[derive(Debug, Default)]
struct Flattened {
    outcomes: Vec<(String, IngestOutcome)>,
    failed_postings: u64,
    truncated_results: u64,
    failed_sources: Vec<String>,
    failed_source_reasons: BTreeMap<String, String>,
    credits_spent_by_source: BTreeMap<String, u64>,
    new_watermark: HashMap<String, String>,
}

/// Flatten fetch results (design decision 8): a source's outcomes are kept
/// whether its fetch completed or ended early — a partial paid fetch's
/// records are paid for and ARE ingested — while a fetch that failed before
/// returning anything contributes only its failure. Every failure, whole or
/// partial, lands in `failed_sources` and carries its reason.
fn flatten_fetches(fetches: Vec<discovery::SourceFetch>) -> Flattened {
    let mut flat = Flattened::default();
    for fetch in fetches {
        let source = fetch.source;
        match fetch.batch {
            Ok(batch) => {
                for outcome in batch.outcomes {
                    flat.outcomes.push((source.clone(), outcome));
                }
                flat.failed_postings += batch.failed;
                flat.truncated_results += batch.truncated_results;
                if batch.credits_spent > 0 {
                    flat.credits_spent_by_source
                        .insert(source.clone(), batch.credits_spent);
                }
                if let Some(ts) = batch.discovered_at {
                    flat.new_watermark.insert(source.clone(), ts);
                }
                if let Some(reason) = batch.partial_error {
                    flat.failed_sources.push(source.clone());
                    flat.failed_source_reasons.insert(source, reason);
                }
            }
            Err(err) => {
                flat.failed_sources.push(source.clone());
                flat.failed_source_reasons.insert(source, err.to_string());
            }
        }
    }
    flat
}

/// Fold a flattened fetch pass into a run summary (both the real and the
/// dry-run path report the same fetch facts). Takes the flattened pass by
/// reference: the caller moves `outcomes` into the ingest loop and the
/// watermark into the run event.
fn apply_fetch_facts(summary: &mut BatchSummary, flat: &Flattened) {
    summary.failed = flat.failed_postings;
    summary.failed_sources = flat.failed_sources.clone();
    summary.failed_source_reasons = flat.failed_source_reasons.clone();
    summary.truncated_results = flat.truncated_results;
    summary.credits_spent_by_source = flat.credits_spent_by_source.clone();
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
/// authorizes the spend (it is checked first, so a scripted `--json` preview
/// can proceed), or the session cannot prompt (`--json` / non-TTY), in which
/// case it refuses rather than corrupt the output or hang.
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
    if yes {
        return DryRunGate::Proceed;
    }
    if json {
        return DryRunGate::Refuse {
            reason: format!(
                "--dry-run with a paid source ({names}) would spend credits; \
                 a prompt would corrupt --json output — pass --yes to proceed"
            ),
        };
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

/// Derive each source's prior `discovered_at` watermark by folding discovery
/// events in reverse (newest first), retaining the first watermark found per
/// source. Folding per source — rather than reading only the most recent
/// event's whole map — keeps a source's cursor across runs that did not
/// produce a watermark for it (a Remotive-only run, a failed/empty fetch),
/// so the next paid run does not re-fetch the already-seen window (read-only;
/// no writer lock).
fn prior_watermark(paths: &AppPaths) -> Result<HashMap<String, String>> {
    let events = read_only_envelopes(paths.event_log())?;
    let mut watermark = HashMap::new();
    for event in events.iter().rev() {
        if event.event_type == event_type::DISCOVERY {
            let payload: DiscoveryPayload =
                serde_json::from_value(event.payload.clone()).into_diagnostic()?;
            if let Some(map) = payload.discovered_at {
                for (source, ts) in map {
                    watermark.entry(source).or_insert(ts);
                }
            }
        }
    }
    Ok(watermark)
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
    let fetches = discovery::fetch_sources(&sources, &client, config).await;

    // Flatten into (source, outcome) pairs, collecting failed sources (with
    // reasons — whole or partial), per-posting failures, truncation, and the
    // new per-source watermarks. A partial paid batch's records are kept.
    let mut flat = flatten_fetches(fetches);
    let new_watermark = flat.new_watermark.clone();

    let rates = discovery::null_rates(
        flat.outcomes.iter().map(|(_, o)| o),
        config.remote_only,
        config.compensation_floor,
    );

    if args.dry_run {
        // Dry run: read the corpus WITHOUT the writer lock, seed an
        // in-memory store, and run the same decide → append loop against it.
        // Nothing is written; the summary is what a real run would do.
        let events = read_only_envelopes(paths.event_log())?;
        let mut store = MemStore::seeded(events);
        let outcomes = std::mem::take(&mut flat.outcomes);
        let mut summary =
            ingest_outcomes_correlated(&mut store, config, &resume_skills, outcomes, run_id)?;
        apply_fetch_facts(&mut summary, &flat);
        summary.unknown_workplace = rates.unknown_workplace;
        summary.unknown_salary = rates.unknown_salary;
        summary.strict_would_drop = rates.strict_would_drop;
        summary.dry_run = true;
        print_summary(&summary, json)?;
        log_rejections_by_source(&summary);
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
    let outcomes = std::mem::take(&mut flat.outcomes);
    let mut summary =
        ingest_outcomes_correlated(&mut store, config, &resume_skills, outcomes, run_id)?;
    apply_fetch_facts(&mut summary, &flat);
    summary.unknown_workplace = rates.unknown_workplace;
    summary.unknown_salary = rates.unknown_salary;
    summary.strict_would_drop = rates.strict_would_drop;
    append_discovery_event(&mut store, run_id, &summary, new_watermark)?;
    print_summary(&summary, json)?;
    log_rejections_by_source(&summary);
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

/// Emit one structured log per source with client-gate rejections (task 2.7):
/// per-source rejection telemetry, recorded at the run level because gating
/// runs after the per-source fetch span closes.
fn log_rejections_by_source(summary: &BatchSummary) {
    for (source, count) in &summary.rejected_by_source {
        if *count > 0 {
            info!(
                source = %source,
                rejected = count,
                "client-gate rejections by source"
            );
        }
    }
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
        for (source, reason) in &summary.failed_source_reasons {
            println!("    {source}: {reason}");
        }
    }
    if !summary.credits_spent_by_source.is_empty() {
        let spend: Vec<String> = summary
            .credits_spent_by_source
            .iter()
            .map(|(source, credits)| format!("{source} {credits}"))
            .collect();
        println!("  credits spent: {}", spend.join(", "));
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
        let source_name = source.clone();
        outcome.source = source;
        // Re-project: the read model is always `rebuild(log)`, and the prior
        // posting's appends must be visible before this one's identity lookup.
        let events = store.replay()?;
        let projection = projections::rebuild(&events)?;
        let ingested =
            record_ingest_correlated(store, &projection, config, resume_skills, outcome, run_id)?;
        if ingested.rejected.is_some() {
            summary.rejected += 1;
            *summary.rejected_by_source.entry(source_name).or_insert(0) += 1;
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
        failed_source_reasons: if summary.failed_source_reasons.is_empty() {
            None
        } else {
            Some(summary.failed_source_reasons.clone())
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
    fn summary_records_rejections_by_source() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = JsonlEventStore::open(dir.path().join("events.jsonl")).unwrap();
        let config = Config {
            remote_only: true,
            ..Default::default()
        };

        let mut rejected = outcome_with_url("https://example.com/b");
        rejected.extracted.remote = Some(false);

        let summary = ingest_outcomes(
            &mut store,
            &config,
            &[],
            vec![("theirstack".into(), rejected)],
        )
        .unwrap();
        assert_eq!(summary.rejected, 1);
        assert_eq!(summary.rejected_by_source.get("theirstack"), Some(&1));
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

    #[test]
    fn prior_watermark_survives_runs_without_a_theirstack_entry() {
        // The bug this guards: a Remotive-only run (or a failed/empty
        // TheirStack fetch) writes a discovery event with no theirstack
        // watermark. Folding per source must retain the earlier cursor, or
        // the next paid run re-fetches the already-seen window at cost.
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
        {
            let mut store = JsonlEventStore::open(paths.event_log()).unwrap();
            append_discovery_event(
                &mut store,
                Uuid::now_v7(),
                &BatchSummary::default(),
                HashMap::new(),
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
        // --json WITH --yes proceeds: --yes skips the prompt, so there is no
        // prompt to corrupt the JSON contract.
        assert!(matches!(
            dry_run_gate(true, true, true, true, &paid),
            DryRunGate::Proceed
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

    // ── partial-fetch salvage + failure reasons (design decision 8) ──

    fn partial_batch(partial_error: Option<String>) -> discovery::SourceBatch {
        let mut outcome = outcome_with_url("https://example.com/salvaged");
        outcome.extracted.remote = Some(true);
        discovery::SourceBatch {
            outcomes: vec![outcome],
            failed: 0,
            truncated_results: 301,
            discovered_at: Some("2024-01-02T00:00:00Z".into()),
            partial_error,
            credits_spent: 301,
        }
    }

    #[test]
    fn flatten_keeps_partial_batch_records_and_carries_the_reason() {
        // The live incident, abstracted: a paid source fetched (and paid
        // for) records, then hit 402 mid-pagination. The records MUST be
        // kept — they are the point of the salvage rule — while the source
        // still counts as failed with its reason and watermark intact.
        let fetches = vec![discovery::SourceFetch {
            source: "theirstack".into(),
            batch: Ok(partial_batch(Some(
                "fetching https://api.theirstack.com failed with status 402: Required: 301 API credits"
                    .into(),
            ))),
        }];

        let flat = flatten_fetches(fetches);

        assert_eq!(flat.outcomes.len(), 1);
        assert_eq!(flat.outcomes[0].0, "theirstack");
        assert_eq!(flat.truncated_results, 301);
        assert_eq!(flat.failed_sources, vec!["theirstack".to_string()]);
        assert!(flat.failed_source_reasons["theirstack"].contains("402"));
        assert_eq!(flat.credits_spent_by_source["theirstack"], 301);
        assert_eq!(
            flat.new_watermark.get("theirstack").map(String::as_str),
            Some("2024-01-02T00:00:00Z")
        );
    }

    #[test]
    fn flatten_records_whole_source_failure_with_reason() {
        // A failure before any record is fetched contributes no outcomes —
        // but its reason must still land, so the log alone can answer "why".
        let fetches = vec![discovery::SourceFetch {
            source: "remotive".into(),
            batch: Err(miette::miette!("feed down")),
        }];

        let flat = flatten_fetches(fetches);

        assert!(flat.outcomes.is_empty());
        assert_eq!(flat.failed_sources, vec!["remotive".to_string()]);
        assert_eq!(flat.failed_source_reasons["remotive"], "feed down");
    }

    #[test]
    fn flatten_keeps_complete_batches_out_of_failed_sources() {
        let fetches = vec![discovery::SourceFetch {
            source: "remotive".into(),
            batch: Ok(partial_batch(None)),
        }];

        let flat = flatten_fetches(fetches);

        assert_eq!(flat.outcomes.len(), 1);
        assert!(flat.failed_sources.is_empty());
        assert!(flat.failed_source_reasons.is_empty());
    }

    #[test]
    fn apply_fetch_facts_folds_reasons_into_the_summary() {
        let fetches = vec![discovery::SourceFetch {
            source: "theirstack".into(),
            batch: Ok(partial_batch(Some("status 402: out of credits".into()))),
        }];
        let flat = flatten_fetches(fetches);

        let mut summary = BatchSummary::default();
        apply_fetch_facts(&mut summary, &flat);

        let value = serde_json::to_value(&summary).unwrap();
        assert_eq!(
            value["failed_source_reasons"]["theirstack"],
            "status 402: out of credits"
        );
        assert_eq!(value["truncated_results"], 301);
        assert_eq!(value["credits_spent_by_source"]["theirstack"], 301);
    }

    #[test]
    fn run_event_roundtrips_failure_reasons() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = JsonlEventStore::open(dir.path().join("events.jsonl")).unwrap();
        let mut summary = BatchSummary {
            failed: 1,
            ..Default::default()
        };
        summary.failed_sources = vec!["theirstack".into()];
        summary
            .failed_source_reasons
            .insert("theirstack".into(), "status 402: out of credits".into());

        append_discovery_event(&mut store, Uuid::now_v7(), &summary, HashMap::new()).unwrap();

        let events = store.replay().unwrap();
        let discovery = events
            .iter()
            .find(|e| e.event_type == event_type::DISCOVERY)
            .expect("a discovery event");
        let payload: DiscoveryPayload = serde_json::from_value(discovery.payload.clone()).unwrap();
        assert_eq!(
            payload
                .failed_source_reasons
                .as_ref()
                .and_then(|m| m.get("theirstack"))
                .map(String::as_str),
            Some("status 402: out of credits")
        );
        // A run where nothing failed serializes no reasons field at all —
        // old readers see the exact shape they always did.
        let bare = DiscoveryPayload {
            new: 0,
            updated: 0,
            suppressed: 0,
            rejected: 0,
            failed: 0,
            failed_sources: vec![],
            discovered_at: None,
            failed_source_reasons: None,
        };
        assert!(
            serde_json::to_value(&bare)
                .unwrap()
                .get("failed_source_reasons")
                .is_none()
        );
    }
}
