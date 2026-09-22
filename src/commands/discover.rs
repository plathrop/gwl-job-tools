//! `gwl-jobs discover`: run the discovery layer (OpenSpec change
//! `discovery-ingestion`) — fetch postings from feed sources and ingest them
//! through the existing pipeline.

use miette::{IntoDiagnostic, Result, miette};
use serde::Serialize;
use tracing::{info, instrument};

use super::{open_workspace, record_ingest};
use crate::{
    cli::DiscoverArgs,
    config::{AppPaths, Config},
    discovery::{self, Posting},
    event_store::EventStore,
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
    let sources = discovery::build_sources(config, args.source.as_deref())?;

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

    // Fetch sources (no writer lock); a source failure is non-fatal.
    let fetches = discovery::fetch_sources(&sources, &client).await;

    // Flatten into (source, posting) pairs, collecting failed sources.
    let mut postings: Vec<(String, Posting)> = Vec::new();
    let mut failed_sources: Vec<String> = Vec::new();
    for fetch in fetches {
        match fetch.postings {
            Ok(ps) => {
                for p in ps {
                    postings.push((fetch.source.clone(), p));
                }
            }
            Err(_) => failed_sources.push(fetch.source),
        }
    }

    // Fetch + extract postings (no lock); a posting failure is non-fatal.
    let (outcomes, failed_postings) = discovery::fetch_postings(postings, &client).await;

    // Acquire the single-writer lock only for the decide → append cycle.
    let (mut store, _projection) = open_workspace(paths)?;
    let mut summary = ingest_outcomes(&mut store, config, &resume_skills, outcomes)?;
    summary.failed = failed_postings;
    summary.failed_sources = failed_sources;

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&summary).into_diagnostic()?
        );
    } else {
        println!("discovery complete");
        println!("  new:        {}", summary.new);
        println!("  updated:    {}", summary.updated);
        println!("  suppressed: {}", summary.suppressed);
        println!("  rejected:   {}", summary.rejected);
        println!("  failed:     {}", summary.failed);
        if !summary.failed_sources.is_empty() {
            println!("  failed sources: {}", summary.failed_sources.join(", "));
        }
    }
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
    let mut summary = BatchSummary::default();
    for (source, mut outcome) in outcomes {
        outcome.source = source;
        // Re-project: the read model is always `rebuild(log)`, and the prior
        // posting's appends must be visible before this one's identity lookup.
        let events = store.replay()?;
        let projection = projections::rebuild(&events)?;
        let ingested = record_ingest(store, &projection, config, resume_skills, outcome)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        cli::Mark,
        commands::{mark_lead, test_support::*},
        config::Config,
        event_store::JsonlEventStore,
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
}
