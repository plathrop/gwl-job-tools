//! End-to-end integration tests: exercise the full command layer
//! (CLI parse → dispatch → event store → projection) against a real
//! JSONL event log in a temp corpus, with no network.
//!
//! These complement the per-module unit tests: they drive the same
//! `cli::execute` entry point the binary uses, so a regression in the
//! dispatch wiring, the `--data-dir` routing, or the cross-command
//! lifecycle is caught here even when every unit test is green.

use std::path::{Path, PathBuf};

use clap::Parser;
use gwl_job_tools::{
    cli::{self, Cli},
    config::{AppPaths, Config},
    event_store::{EventStore, JsonlEventStore},
    projections::{self, LeadRecord, Projection},
};
use miette::Result;

/// Path to a checked-in fixture under `tests/fixtures/`.
fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// A fresh temp corpus: an `AppPaths` whose data dir is a temp directory,
/// so every test runs against an isolated event log.
fn setup() -> (tempfile::TempDir, AppPaths, Config) {
    let dir = tempfile::tempdir().unwrap();
    let paths = AppPaths::new(dir.path().join("config"), dir.path().join("data"));
    (dir, paths, Config::default())
}

/// Run a command through the same parse → dispatch path as the binary.
async fn run(paths: &AppPaths, config: &Config, args: &[&str]) -> Result<()> {
    let mut full = vec!["gwl-jobs"];
    full.extend_from_slice(args);
    let cli = Cli::try_parse_from(full).expect("valid CLI invocation");
    cli::execute(cli.command, config, paths, cli.json, false).await
}

/// Rebuild the projection from the on-disk event log (the source of truth).
fn projection(paths: &AppPaths) -> Projection {
    let store = JsonlEventStore::open(paths.data_dir().join("events.jsonl")).unwrap();
    projections::rebuild(&store.replay().unwrap()).unwrap()
}

/// The single lead in a corpus, or panic with a useful message.
fn only_lead(proj: &Projection) -> &LeadRecord {
    assert_eq!(proj.leads.len(), 1, "expected exactly one lead");
    proj.leads.values().next().unwrap()
}

#[tokio::test]
async fn ingest_file_creates_lead_with_extracted_fields() {
    let (_dir, paths, config) = setup();
    let fixture = fixture("northwind-staff-backend.html");

    run(
        &paths,
        &config,
        &["ingest", "--file", fixture.to_str().unwrap()],
    )
    .await
    .unwrap();

    let proj = projection(&paths);
    let lead = only_lead(&proj);
    assert_eq!(lead.adapter.as_deref(), Some("drop-in"));
    assert_eq!(lead.extracted.company.as_deref(), Some("Northwind Labs"));
    assert_eq!(lead.extracted.req_id.as_deref(), Some("NW-2026-042"));
    assert_eq!(lead.extracted.remote, Some(true));
    let comp = lead.extracted.comp.as_ref().expect("comp extracted");
    assert_eq!(comp.min, Some(180_000));
    assert_eq!(comp.max, Some(240_000));
    // A gate-passing ingest is scored (decision 0006).
    assert!(lead.latest_score.is_some());
}

#[tokio::test]
async fn full_lifecycle_ingest_edit_mark_outcome() {
    let (_dir, paths, config) = setup();
    let fixture = fixture("northwind-staff-backend.html");

    run(
        &paths,
        &config,
        &["ingest", "--file", fixture.to_str().unwrap()],
    )
    .await
    .unwrap();

    let lead_id = only_lead(&projection(&paths)).lead_id.to_string();

    // Edit: correct the title.
    run(
        &paths,
        &config,
        &[
            "edit",
            &lead_id,
            "--title",
            "Staff Backend Engineer, Platform",
        ],
    )
    .await
    .unwrap();
    let proj = projection(&paths);
    let lead = only_lead(&proj);
    assert_eq!(
        lead.extracted.title.as_deref(),
        Some("Staff Backend Engineer, Platform")
    );
    // The edit flips the adapter drop-in → user (decision record 0009).
    assert_eq!(lead.adapter.as_deref(), Some("user"));

    // Mark: decide to apply manually.
    run(&paths, &config, &["mark", &lead_id, "apply-manual"])
        .await
        .unwrap();
    let proj = projection(&paths);
    assert_eq!(
        only_lead(&proj).latest_mark.as_deref(),
        Some("apply-manual")
    );

    // Outcome: terminal rejection removes the lead from the active pipeline.
    run(
        &paths,
        &config,
        &["outcome", &lead_id, "rejected_by_employer"],
    )
    .await
    .unwrap();
    let proj = projection(&paths);
    let lead = only_lead(&proj);
    assert_eq!(
        lead.latest_outcome.as_ref().map(|o| o.event_type.as_str()),
        Some("rejected_by_employer")
    );
    assert!(proj.active_leads().is_empty());
}

#[tokio::test]
async fn reingest_same_file_dedupes() {
    let (_dir, paths, config) = setup();
    let fixture = fixture("northwind-staff-backend.html");
    let args = ["ingest", "--file", fixture.to_str().unwrap()];

    run(&paths, &config, &args).await.unwrap();
    run(&paths, &config, &args).await.unwrap();

    // Same posting, same identity → one lead, not two.
    assert_eq!(projection(&paths).leads.len(), 1);
}

#[tokio::test]
async fn data_dir_isolation() {
    // Two corpora must not observe each other's leads.
    let (_dir_a, paths_a, config) = setup();
    let (_dir_b, paths_b, _) = setup();
    let fixture = fixture("northwind-staff-backend.html");

    run(
        &paths_a,
        &config,
        &["ingest", "--file", fixture.to_str().unwrap()],
    )
    .await
    .unwrap();

    assert_eq!(projection(&paths_a).leads.len(), 1);
    assert!(projection(&paths_b).leads.is_empty());
}
