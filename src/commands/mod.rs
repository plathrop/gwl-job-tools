//! Command implementations. Thin: I/O wiring around the domain.

use miette::{Context, IntoDiagnostic, Result, miette};
use serde::Serialize;
use serde_with::skip_serializing_none;
use tracing::{info, instrument};
use url::Url;
use uuid::Uuid;

use crate::{
    cli::{IngestArgs, ShowArgs},
    config::{AppPaths, Config},
    domain::{
        events::ExtractedFields,
        gates::{self, GateFailure},
        identity::{self, compute_identity},
        lead::{self, IngestKind, LeadState},
        scoring::{self, ScoreResult},
    },
    event_store::{EventStore, JsonlEventStore},
    ingest::{self, IngestOutcome},
    projections::{self, LeadRecord, Projection},
    resume,
};

const EVENT_LOG_NAME: &str = "events.jsonl";

#[instrument(skip_all)]
pub async fn execute_ingest(args: IngestArgs, config: &Config, paths: &AppPaths) -> Result<()> {
    // Load the resume before fetching: a configured-but-broken resume fails
    // loudly before any network I/O (decision 0004).
    let resume = resume::load(config.resume_path.as_deref())?;
    let resume_skills = resume
        .as_ref()
        .map(resume::Resume::keywords)
        .unwrap_or_default();

    // Fetch/extract *before* acquiring the single-writer lock: network waits
    // must not hold the lock (durability contract, design doc 0001 §1).
    let outcome = match (&args.url, &args.file) {
        (Some(url), None) => {
            let http = ingest::default_client()?;
            ingest::ingest_url(url, &http).await?
        }
        (None, Some(path)) => {
            let content = std::fs::read_to_string(path)
                .into_diagnostic()
                .wrap_err_with(|| format!("reading {}", path.display()))?;
            ingest::ingest_file(path, &content)?
        }
        _ => return Err(miette!("exactly one of <url> or --file <path> is required")),
    };

    // Acquire the lock only for the fast read → decide → append cycle.
    let mut store = JsonlEventStore::open(paths.data_dir().join(EVENT_LOG_NAME))?;
    let events = store.replay()?;
    let projection = projections::rebuild(&events)?;

    let summary = record_ingest(&mut store, &projection, config, &resume_skills, outcome)?;
    println!(
        "{}",
        serde_json::to_string_pretty(&summary).into_diagnostic()?
    );
    Ok(())
}

/// The testable core of ingest: given an extraction outcome, match identity
/// against the projection, decide events (including gate evaluation), and
/// append them. No network, no filesystem beyond the store itself.
pub fn record_ingest(
    store: &mut impl EventStore,
    projection: &Projection,
    config: &Config,
    resume_skills: &[String],
    outcome: IngestOutcome,
) -> Result<IngestSummary> {
    // Store the canonical URL (tracking params stripped) so re-ingests via
    // differently-tagged links don't record spurious `url` changes.
    let canonical_url = outcome
        .url
        .as_deref()
        .and_then(|u| Url::parse(u).ok())
        .map(|u| identity::canonicalize_url(&u));
    let identity = compute_identity(
        &outcome.extracted,
        canonical_url
            .as_deref()
            .and_then(|u| Url::parse(u).ok())
            .as_ref(),
        &outcome.raw_text,
    );

    let correlation_id = Uuid::now_v7();
    let (lead_id, state) = match projection.lookup(&identity) {
        Some(lead_id) => {
            let stream = LeadState::stream_id(lead_id);
            let mut state = LeadState::default();
            for event in store.load(&stream)? {
                lead::evolve(&mut state, &event);
            }
            (lead_id, state)
        }
        None => (Uuid::now_v7(), LeadState::default()),
    };

    // Gates evaluate the new snapshot; failures become `rejected` events in
    // the same batch as the snapshot event (suppressed re-ingests skip
    // gating entirely, per the durable-ignore rule). A gate-passing
    // evaluation is scored; `scored` is the pass-marker (decision 0006).
    let suppressed = state.latest_mark.as_deref() == Some("ignore");
    let gate_failures = if suppressed {
        Vec::new()
    } else {
        gates::evaluate(config, &outcome.extracted, &outcome.raw_text)
    };
    let score = if !suppressed && gate_failures.is_empty() {
        Some(scoring::score(
            config,
            &outcome.extracted,
            &outcome.raw_text,
            resume_skills,
        ))
    } else {
        None
    };

    let (kind, pending) = lead::decide_ingest(
        &state,
        &identity,
        &outcome.source,
        canonical_url.clone(),
        outcome.raw_text,
        outcome.extracted.clone(),
        gate_failures.clone(),
        score.clone(),
    )?;

    let stream = LeadState::stream_id(lead_id);
    store.append(&stream, state.seq, &pending, correlation_id)?;

    match &kind {
        IngestKind::New => info!(%lead_id, "lead ingested"),
        IngestKind::Updated { changed } => info!(%lead_id, ?changed, "lead updated"),
        IngestKind::Suppressed => info!(%lead_id, "re-ingest suppressed (ignored lead)"),
    }

    Ok(IngestSummary {
        lead_id,
        kind: match &kind {
            IngestKind::New => "ingested",
            IngestKind::Updated { .. } => "updated",
            IngestKind::Suppressed => "reingest_suppressed",
        },
        changed: match &kind {
            IngestKind::Updated { changed } => Some(changed.clone()),
            _ => None,
        },
        rejected: if gate_failures.is_empty() {
            None
        } else {
            Some(gate_failures)
        },
        score,
        dedupe_key: identity.dedupe_key,
        source: outcome.source,
        url: canonical_url,
        extracted: outcome.extracted,
    })
}

#[skip_serializing_none]
#[derive(Clone, Debug, Serialize)]
pub struct IngestSummary {
    pub lead_id: Uuid,
    pub kind: &'static str,
    pub changed: Option<Vec<String>>,
    pub rejected: Option<Vec<GateFailure>>,
    pub score: Option<ScoreResult>,
    pub dedupe_key: String,
    pub source: String,
    pub url: Option<String>,
    pub extracted: ExtractedFields,
}

#[instrument(skip_all)]
pub async fn execute_show(args: ShowArgs, paths: &AppPaths) -> Result<()> {
    let store = JsonlEventStore::open(paths.data_dir().join(EVENT_LOG_NAME))?;
    let events = store.replay()?;
    let projection = projections::rebuild(&events)?;

    let record = select_lead(&projection, &args.id)?;
    println!(
        "{}",
        serde_json::to_string_pretty(record).into_diagnostic()?
    );
    Ok(())
}

/// Resolve a `<lead>` argument (unambiguous UUID prefix, design doc §8) to a
/// single projection record.
pub fn select_lead<'a>(projection: &'a Projection, prefix: &str) -> Result<&'a LeadRecord> {
    match projection.find_by_id_prefix(prefix).as_slice() {
        [] => Err(miette!("no lead matches id prefix '{prefix}'")),
        [record] => Ok(record),
        many => Err(miette!(
            "id prefix '{}' is ambiguous (matches {} leads: {})",
            prefix,
            many.len(),
            many.iter()
                .map(|r| r.lead_id.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(raw_text: &str) -> IngestOutcome {
        IngestOutcome {
            source: "drop-in".into(),
            url: None,
            raw_text: raw_text.into(),
            extracted: ExtractedFields::default(),
        }
    }

    fn store_and_projection(dir: &tempfile::TempDir) -> (JsonlEventStore, Projection) {
        let store = JsonlEventStore::open(dir.path().join("events.jsonl")).unwrap();
        let projection = projections::rebuild(&store.replay().unwrap()).unwrap();
        (store, projection)
    }

    // ── record_ingest: dedupe → decide → append ──────────────────

    #[test]
    fn unstructured_drop_dedupes_on_reingest() {
        // Regression (Remi's raw-fallback bug): an unstructured posting with
        // no req/url/title/company falls back to a `raw:` dedupe key — and
        // re-ingesting it must match the same lead, not mint a new one.
        let dir = tempfile::tempdir().unwrap();
        let (mut store, projection) = store_and_projection(&dir);

        let first = record_ingest(
            &mut store,
            &projection,
            &Config::default(),
            &[],
            outcome("totally unstructured body"),
        )
        .unwrap();
        assert_eq!(first.kind, "ingested");
        assert!(first.dedupe_key.starts_with("raw:"));

        let projection = projections::rebuild(&store.replay().unwrap()).unwrap();
        let second = record_ingest(
            &mut store,
            &projection,
            &Config::default(),
            &[],
            outcome("totally unstructured body"),
        )
        .unwrap();
        assert_eq!(second.kind, "updated");
        assert_eq!(second.changed, Some(vec![]));
        assert_eq!(second.lead_id, first.lead_id);
    }

    #[test]
    fn reingest_with_tracking_params_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (mut store, projection) = store_and_projection(&dir);
        let mut with_url = outcome("body");
        with_url.url = Some("https://example.com/job/1".into());
        let first =
            record_ingest(&mut store, &projection, &Config::default(), &[], with_url).unwrap();

        let projection = projections::rebuild(&store.replay().unwrap()).unwrap();
        let mut tagged = outcome("body");
        tagged.url = Some("https://example.com/job/1?utm_source=newsletter".into());
        let second =
            record_ingest(&mut store, &projection, &Config::default(), &[], tagged).unwrap();

        assert_eq!(second.kind, "updated");
        assert_eq!(second.changed, Some(vec![]));
        assert_eq!(second.lead_id, first.lead_id);
        // The stored URL is canonical (tracking params stripped).
        assert_eq!(second.url.as_deref(), Some("https://example.com/job/1"));
    }

    #[test]
    fn changed_content_reports_changed_fields() {
        let dir = tempfile::tempdir().unwrap();
        let (mut store, projection) = store_and_projection(&dir);
        let mut first_outcome = outcome("body v1");
        first_outcome.url = Some("https://example.com/job/1".into());
        record_ingest(
            &mut store,
            &projection,
            &Config::default(),
            &[],
            first_outcome,
        )
        .unwrap();

        let projection = projections::rebuild(&store.replay().unwrap()).unwrap();
        let mut second_outcome = outcome("body v2");
        second_outcome.url = Some("https://example.com/job/1".into());
        let second = record_ingest(
            &mut store,
            &projection,
            &Config::default(),
            &[],
            second_outcome,
        )
        .unwrap();

        assert_eq!(second.changed, Some(vec!["raw_text".to_string()]));
    }

    // ── select_lead ──────────────────────────────────────────────

    fn projection_with_leads(dir: &tempfile::TempDir, count: usize) -> Projection {
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

    #[test]
    fn select_lead_unique_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let projection = projection_with_leads(&dir, 1);
        let id = projection.leads.keys().next().unwrap().to_string();
        let record = select_lead(&projection, &id[..8]).unwrap();
        assert_eq!(record.lead_id.to_string(), id);
    }

    #[test]
    fn select_lead_no_match_bails() {
        let projection = projections::rebuild(&[]).unwrap();
        assert!(select_lead(&projection, "deadbeef").is_err());
    }

    #[test]
    fn select_lead_ambiguous_prefix_bails() {
        let dir = tempfile::tempdir().unwrap();
        // Two leads that share a one-character prefix is guaranteed with
        // enough leads; simpler: prefix "" matches everything.
        let projection = projection_with_leads(&dir, 2);
        assert!(select_lead(&projection, "").is_err());
    }

    // ── gate wiring (Increment 2) ────────────────────────────────

    #[test]
    fn gate_failure_appends_rejected_in_same_batch() {
        let dir = tempfile::tempdir().unwrap();
        let (mut store, projection) = store_and_projection(&dir);
        let config = Config {
            remote_only: true,
            ..Default::default()
        };
        let mut outcome = outcome("body");
        outcome.extracted.remote = Some(false);
        outcome.extracted.location = Some("San Francisco, CA".into());
        outcome.extracted.company = Some("Acme".into());
        outcome.extracted.title = Some("Engineer".into());

        let summary = record_ingest(&mut store, &projection, &config, &[], outcome).unwrap();

        assert_eq!(summary.kind, "ingested");
        let rejections = summary.rejected.unwrap();
        assert_eq!(rejections.len(), 1);
        assert_eq!(rejections[0].gate.as_str(), "remote-only");

        // ingested + rejected share one batch: consecutive seqs, one
        // correlation id, rejection caused by the ingested event.
        let events = store.replay().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event_type, "ingested");
        assert_eq!(events[1].event_type, "rejected");
        assert_eq!(events[1].seq, 2);
        assert_eq!(events[1].correlation_id, events[0].correlation_id);
        assert_eq!(events[1].causation_id, Some(events[0].id));
        assert_eq!(events[1].payload["revision"], 1);

        // The projection records the rejection.
        let projection = projections::rebuild(&events).unwrap();
        let record = projection.leads.get(&summary.lead_id).unwrap();
        let rejection = record.latest_rejection.as_ref().unwrap();
        assert_eq!(rejection.gate, "remote-only");
    }

    #[test]
    fn reingest_after_fix_passing_gates_clears_rejection() {
        let dir = tempfile::tempdir().unwrap();
        let (mut store, projection) = store_and_projection(&dir);
        let config = Config {
            remote_only: true,
            ..Default::default()
        };
        let mut onsite = outcome("body");
        onsite.url = Some("https://example.com/job/1".into());
        onsite.extracted.remote = Some(false);
        let first = record_ingest(&mut store, &projection, &config, &[], onsite).unwrap();
        assert!(first.rejected.is_some());

        // Repost: same job, now marked remote.
        let projection = projections::rebuild(&store.replay().unwrap()).unwrap();
        let mut repost = outcome("body");
        repost.url = Some("https://example.com/job/1".into());
        repost.extracted.remote = Some(true);
        let second = record_ingest(&mut store, &projection, &config, &[], repost).unwrap();
        assert_eq!(second.kind, "updated");
        assert!(second.rejected.is_none());

        let events = store.replay().unwrap();
        let projection = projections::rebuild(&events).unwrap();
        let record = projection.leads.get(&first.lead_id).unwrap();
        assert!(record.latest_rejection.is_none());
    }

    #[test]
    fn reingest_failing_different_gate_updates_rejection() {
        let dir = tempfile::tempdir().unwrap();
        let (mut store, projection) = store_and_projection(&dir);
        let config = Config {
            remote_only: true,
            compensation_floor: Some(180_000),
            ..Default::default()
        };
        let mut onsite = outcome("body");
        onsite.url = Some("https://example.com/job/1".into());
        onsite.extracted.remote = Some(false);
        let first = record_ingest(&mut store, &projection, &config, &[], onsite).unwrap();
        assert_eq!(first.rejected.unwrap()[0].gate.as_str(), "remote-only");

        // Repost: now remote, but comp below floor — the rejection should
        // move to the new gate, not just clear.
        let projection = projections::rebuild(&store.replay().unwrap()).unwrap();
        let mut repost = outcome("body v2");
        repost.url = Some("https://example.com/job/1".into());
        repost.extracted.remote = Some(true);
        repost.extracted.comp = Some(crate::domain::events::CompRange {
            min: Some(120_000),
            max: Some(140_000),
            currency: "USD".into(),
            period: "year".into(),
            raw: "$120,000 - $140,000".into(),
        });
        let second = record_ingest(&mut store, &projection, &config, &[], repost).unwrap();
        assert_eq!(
            second.rejected.unwrap()[0].gate.as_str(),
            "compensation-floor"
        );

        let projection = projections::rebuild(&store.replay().unwrap()).unwrap();
        let record = projection.leads.get(&first.lead_id).unwrap();
        let rejection = record.latest_rejection.as_ref().unwrap();
        assert_eq!(rejection.gate, "compensation-floor");
        assert_eq!(rejection.revision, 2);
    }

    #[test]
    fn blacklisted_company_rejected_via_file_drop() {
        // Pipeline-level: the blacklist gate must hold on file drops too,
        // via the title-derived company fallback ("never match blacklisted
        // companies" is non-negotiable on every ingest path).
        let dir = tempfile::tempdir().unwrap();
        let (mut store, projection) = store_and_projection(&dir);
        let config = Config {
            blacklist: vec!["salesforce".into()],
            ..Default::default()
        };
        let html = "<html><head><title>Staff Engineer — Salesforce</title></head><body>
                    <article><h1>Staff Engineer</h1>
                    <p>Join our platform team to build delightful CRM tooling with
                    a large team of experienced engineers across the organization
                    every single day of the week.</p>
                    </article></body></html>";
        let outcome = crate::ingest::ingest_file(std::path::Path::new("jd.html"), html).unwrap();
        assert_eq!(outcome.extracted.company.as_deref(), Some("Salesforce"));

        let summary = record_ingest(&mut store, &projection, &config, &[], outcome).unwrap();
        let rejections = summary.rejected.unwrap();
        assert_eq!(rejections[0].gate.as_str(), "blacklist");
    }

    // ── scored wiring (Increment 3) ───────────────────────────────

    #[test]
    fn scored_event_carries_batch_metadata() {
        // A passing ingest appends ingested + scored in one batch: same
        // correlation id, scored caused by the snapshot, revision 1.
        let dir = tempfile::tempdir().unwrap();
        let (mut store, projection) = store_and_projection(&dir);
        let mut outcome = outcome("body");
        outcome.url = Some("https://example.com/job/10".into());
        outcome.extracted.title = Some("Staff Engineer".into());
        outcome.extracted.company = Some("Acme".into());

        record_ingest(&mut store, &projection, &Config::default(), &[], outcome).unwrap();

        let events = store.replay().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[1].event_type, "scored");
        assert_eq!(events[1].seq, 2);
        assert_eq!(events[1].correlation_id, events[0].correlation_id);
        assert_eq!(events[1].causation_id, Some(events[0].id));
        assert_eq!(events[1].payload["revision"], 1);
    }

    #[test]
    fn resume_skills_flow_into_skills_dimension() {
        // Every other record_ingest test passes an empty skill list; this one
        // exercises the skills-present path end to end.
        let dir = tempfile::tempdir().unwrap();
        let (mut store, projection) = store_and_projection(&dir);
        let mut outcome = outcome("We use Kubernetes heavily.");
        outcome.url = Some("https://example.com/job/11".into());
        outcome.extracted.title = Some("Staff Engineer".into());
        outcome.extracted.company = Some("Acme".into());

        let summary = record_ingest(
            &mut store,
            &projection,
            &Config::default(),
            &["Kubernetes".to_string()],
            outcome,
        )
        .unwrap();

        let score = summary.score.as_ref().unwrap();
        let skills = score
            .dimensions
            .iter()
            .find(|d| d.name == "skills")
            .unwrap();
        assert_eq!(skills.score, 10);
        // level 100 + skills 10 + remote 50 (unknown), comp dropped, equal
        // weights → 160/3 = 53.
        assert_eq!(score.composite, 53);
    }

    // ── golden round-trip (design doc §10) ───────────────────────

    /// Write → replay → projection equality, through the real store.
    #[test]
    fn golden_log_replay_projection() {
        let dir = tempfile::tempdir().unwrap();
        let (mut store, projection) = store_and_projection(&dir);

        let mut outcome = outcome("body");
        outcome.url = Some("https://example.com/job/9".into());
        outcome.extracted.title = Some("Staff Engineer".into());
        outcome.extracted.company = Some("Acme".into());
        outcome.extracted.req_id = Some("R-9".into());
        let summary =
            record_ingest(&mut store, &projection, &Config::default(), &[], outcome).unwrap();

        let replayed = store.replay().unwrap();
        assert_eq!(replayed.len(), 2);
        assert_eq!(replayed[0].seq, 1);
        assert_eq!(replayed[0].event_type, "ingested");
        assert_eq!(replayed[1].seq, 2);
        assert_eq!(replayed[1].event_type, "scored");
        let projection = projections::rebuild(&replayed).unwrap();
        let record = projection.leads.get(&summary.lead_id).unwrap();
        assert_eq!(record.extracted.title.as_deref(), Some("Staff Engineer"));
        assert_eq!(record.dedupe_key.as_deref(), Some("req:acme:r-9"));
        // The scored event is projected as the latest score (level 100 +
        // remote 50, equal weights → 75).
        let score = record.latest_score.as_ref().unwrap();
        assert_eq!(score.composite, 75);
    }
}
