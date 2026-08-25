//! Command implementations. Thin: I/O wiring around the domain.

use miette::{Context, IntoDiagnostic, Result, bail, miette};
use serde::Serialize;
use serde_with::skip_serializing_none;
use tracing::{info, instrument};
use url::Url;
use uuid::Uuid;

use crate::cli::{IngestArgs, ShowArgs};
use crate::config::{AppPaths, Config};
use crate::domain::events::ExtractedFields;
use crate::domain::identity::{self, compute_identity};
use crate::domain::lead::{self, IngestKind, LeadState};
use crate::event_store::{EventStore, JsonlEventStore};
use crate::ingest::{self, HttpClient};
use crate::projections::{self, Projection};

const EVENT_LOG_NAME: &str = "events.jsonl";

#[instrument(skip_all)]
pub async fn execute_ingest(args: IngestArgs) -> Result<()> {
    let paths = AppPaths::discover()?;
    let _config = Config::load(&paths)?;

    // Acquire the single-writer lock for the whole read→decide→append cycle
    // (durability contract, design doc 0001 §1).
    let mut store = JsonlEventStore::open(paths.data_dir().join(EVENT_LOG_NAME))?;
    let events = store.replay()?;
    let projection = projections::rebuild(&events);

    let outcome = match (&args.url, &args.file) {
        (Some(url), None) => {
            let http = HttpClient::new()?;
            ingest::ingest_url(url, &http).await?
        }
        (None, Some(path)) => {
            let content = std::fs::read_to_string(path)
                .into_diagnostic()
                .wrap_err_with(|| format!("reading {}", path.display()))?;
            ingest::ingest_file(path, &content)?
        }
        _ => bail!("exactly one of <url> or --file <path> is required"),
    };

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

    let (kind, pending) = lead::decide_ingest(
        &state,
        &identity,
        &outcome.source,
        canonical_url.clone(),
        outcome.raw_text,
        outcome.extracted.clone(),
    )?;

    let stream = LeadState::stream_id(lead_id);
    store.append(&stream, state.seq, &pending, correlation_id)?;

    match &kind {
        IngestKind::New => info!(%lead_id, "lead ingested"),
        IngestKind::Updated { changed } => info!(%lead_id, ?changed, "lead updated"),
        IngestKind::Suppressed => info!(%lead_id, "re-ingest suppressed (ignored lead)"),
    }

    let summary = IngestSummary {
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
        dedupe_key: identity.dedupe_key,
        source: outcome.source,
        url: canonical_url,
        extracted: outcome.extracted,
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&summary).into_diagnostic()?
    );
    Ok(())
}

#[skip_serializing_none]
#[derive(Serialize)]
struct IngestSummary {
    lead_id: Uuid,
    kind: &'static str,
    changed: Option<Vec<String>>,
    dedupe_key: String,
    source: String,
    url: Option<String>,
    extracted: ExtractedFields,
}

#[instrument(skip_all)]
pub async fn execute_show(args: ShowArgs) -> Result<()> {
    let paths = AppPaths::discover()?;
    let store = JsonlEventStore::open(paths.data_dir().join(EVENT_LOG_NAME))?;
    let events = store.replay()?;
    let projection = projections::rebuild(&events);

    let matches = projection.find_by_id_prefix(&args.id);
    match matches.as_slice() {
        [] => bail!("no lead matches id prefix '{}'", args.id),
        [record] => {
            println!(
                "{}",
                serde_json::to_string_pretty(record).into_diagnostic()?
            );
            Ok(())
        }
        many => Err(miette!(
            "id prefix '{}' is ambiguous (matches {} leads: {})",
            args.id,
            many.len(),
            many.iter()
                .map(|r| r.lead_id.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// Read access for tests and future commands.
#[allow(dead_code)]
pub fn load_projection(paths: &AppPaths) -> Result<Projection> {
    let store = JsonlEventStore::open(paths.data_dir().join(EVENT_LOG_NAME))?;
    Ok(projections::rebuild(&store.replay()?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event_store::EventStore as _;

    /// Golden round-trip: append events through the store, replay, and
    /// verify the projection reflects them (design doc 0001 §10).
    #[test]
    fn golden_log_replay_projection() {
        let dir = tempfile::tempdir().unwrap();
        let paths = AppPaths::new(dir.path().join("config"), dir.path().join("data"));
        let log_path = paths.data_dir().join(EVENT_LOG_NAME);

        let lead_id = Uuid::now_v7();
        let stream = LeadState::stream_id(lead_id);
        let mut store = JsonlEventStore::open(&log_path).unwrap();

        let state = LeadState::default();
        let identity = crate::domain::identity::compute_identity(
            &ExtractedFields {
                title: Some("Staff Engineer".into()),
                company: Some("Acme".into()),
                ..Default::default()
            },
            None,
            "body",
        );
        let (_, pending) = lead::decide_ingest(
            &state,
            &identity,
            "drop-in",
            None,
            "body".into(),
            ExtractedFields {
                title: Some("Staff Engineer".into()),
                company: Some("Acme".into()),
                ..Default::default()
            },
        )
        .unwrap();
        store.append(&stream, 0, &pending, Uuid::now_v7()).unwrap();

        // Replay from disk and project.
        let replayed = store.replay().unwrap();
        assert_eq!(replayed.len(), 1);
        let projection = projections::rebuild(&replayed);
        let record = projection.leads.get(&lead_id).unwrap();
        assert_eq!(record.extracted.title.as_deref(), Some("Staff Engineer"));
        assert_eq!(
            projection.lookup(&crate::domain::identity::compute_identity(
                &ExtractedFields {
                    title: Some("staff engineer".into()),
                    company: Some("ACME".into()),
                    ..Default::default()
                },
                None,
                "different body",
            )),
            Some(lead_id)
        );
    }
}
