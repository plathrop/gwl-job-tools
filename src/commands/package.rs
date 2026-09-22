use miette::{IntoDiagnostic, Result};
use tracing::{info, instrument, warn};
use uuid::Uuid;

use super::{open_url, open_workspace, replay_lead, select_lead};
use crate::{
    cli::PackageArgs,
    config::{AppPaths, Config},
    domain::{
        events::{ApplyPackage, ApplyQueuedPayload, CheatSheetEntry},
        lead::{self, LeadState},
    },
    event_store::EventStore,
    projections::LeadRecord,
    render,
    resume::{self, Resume},
};

#[instrument(skip_all)]
pub async fn execute_package(
    args: PackageArgs,
    config: &Config,
    paths: &AppPaths,
    json: bool,
) -> Result<()> {
    let (mut store, projection) = open_workspace(paths)?;

    let record = select_lead(&projection, &args.lead)?;
    let lead_id = record.lead_id;
    // Assemble the package BEFORE the guard can fail (a broken resume fails
    // loudly, decision 0004, and the lead stays untouched); the guard then
    // refuses unmarked leads without appending anything.
    let apply = prepare_package(config, record)?;
    let state = replay_lead(&store, lead_id)?;
    let pending = lead::decide_package(&state, &apply)?;
    let stream = LeadState::stream_id(lead_id);
    store.append(&stream, state.seq, &[pending], Uuid::now_v7())?;
    info!(%lead_id, "apply package re-prepared and re-opened");

    if json {
        let output = serde_json::json!({
            "lead_id": lead_id,
            "package": apply.package,
            "url": apply.url,
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&output).into_diagnostic()?
        );
    } else {
        print!("{}", render::render_cheat_sheet(&apply.package.cheat_sheet));
    }
    // Re-open the posting URL (best-effort, like the review loop's `a`
    // key; the final click is the user's). The machine-readable --json path
    // must not spawn a browser on the operator's display (PR #17 review:
    // verified live with a stubbed xdg-open).
    if !json && let Some(url) = record.url.as_deref() {
        println!("  URL: {}", render::sanitize(url));
        open_url(url);
    }
    Ok(())
}

/// Assemble the apply package for an `apply-automatically` lead (design doc
/// 0001 §3): cover-letter path, resume PDF path (derived from the JSON
/// resume path), the ATS cheat sheet, and the posting URL. Fails loudly on a
/// configured-but-broken resume (decision 0004); a missing resume degrades
/// to an empty cheat sheet. A configured-but-missing cover letter or derived
/// resume PDF is a warning, not a failure — the files are attached manually
/// in v0.
pub(crate) fn prepare_package(config: &Config, record: &LeadRecord) -> Result<ApplyQueuedPayload> {
    let resume = resume::load(config.resume_path.as_deref())?;
    let cheat_sheet = resume.as_ref().map(cheat_sheet).unwrap_or_default();

    let cover_letter_path = config
        .cover_letter_path
        .as_ref()
        .map(|p| p.to_string_lossy().into_owned());
    if let Some(path) = config.cover_letter_path.as_deref()
        && !path.exists()
    {
        warn!(path = %path.display(), "configured cover letter does not exist");
    }

    let resume_pdf = config
        .resume_path
        .as_deref()
        .map(|p| p.with_extension("pdf"));
    if let Some(pdf) = resume_pdf.as_deref()
        && !pdf.exists()
    {
        warn!(path = %pdf.display(), "derived resume PDF does not exist");
    }

    Ok(ApplyQueuedPayload {
        package: ApplyPackage {
            cover_letter_path,
            resume_path: resume_pdf.map(|p| p.to_string_lossy().into_owned()),
            cheat_sheet,
        },
        url: record.url.clone(),
    })
}

/// The ATS answers cheat sheet (design doc 0001 §3): a static list of common
/// ATS questions with resume-derived answers. Best-effort — a question whose
/// answer isn't in the resume is omitted.
pub(crate) fn cheat_sheet(resume: &Resume) -> Vec<CheatSheetEntry> {
    let mut entries = Vec::new();
    if let Some(name) = resume.basics.name.as_deref() {
        entries.push(CheatSheetEntry {
            question: "Full name".into(),
            answer: name.into(),
        });
    }
    if let Some(email) = resume.basics.email.as_deref() {
        entries.push(CheatSheetEntry {
            question: "Email address".into(),
            answer: email.into(),
        });
    }
    if let Some(phone) = resume.basics.phone.as_deref() {
        entries.push(CheatSheetEntry {
            question: "Phone number".into(),
            answer: phone.into(),
        });
    }
    if let Some(loc) = &resume.basics.location {
        let parts: Vec<&str> = [
            loc.city.as_deref(),
            loc.region.as_deref(),
            loc.country_code.as_deref(),
        ]
        .into_iter()
        .flatten()
        .collect();
        if !parts.is_empty() {
            entries.push(CheatSheetEntry {
                question: "Location".into(),
                answer: parts.join(", "),
            });
        }
    }
    if let Some(work) = resume.work.first() {
        if let Some(position) = work.position.as_deref() {
            entries.push(CheatSheetEntry {
                question: "Current or most recent title".into(),
                answer: position.into(),
            });
        }
        if let Some(company) = work.name.as_deref() {
            entries.push(CheatSheetEntry {
                question: "Current or most recent employer".into(),
                answer: company.into(),
            });
        }
    }
    entries
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        cli::Mark,
        commands::{mark_lead, opened_urls, test_support::*},
        domain::events::ExtractedFields,
        event_store::JsonlEventStore,
    };

    // ── package re-entry (Increment 5) ────────────────────────────

    #[tokio::test]
    async fn execute_package_rebuilds_for_marked_lead() {
        // The re-entry case: an apply-automatically lead whose browser tab
        // is long gone. `package` re-prepares, appends a fresh apply_queued,
        // and (in the real run) re-opens the URL.
        let dir = tempfile::tempdir().unwrap();
        let (mut store, record) = ingested_lead(
            &dir,
            &Config::default(),
            ExtractedFields {
                title: Some("Engineer".into()),
                company: Some("Acme".into()),
                ..Default::default()
            },
            "body",
        );
        // Mark first — the mark is the approval.
        mark_lead(
            &mut store,
            &Config::default(),
            &record,
            Mark::ApplyAutomatically,
            None,
        )
        .unwrap();
        // One apply_queued from the mark itself.
        assert_eq!(
            store
                .replay()
                .unwrap()
                .iter()
                .filter(|e| e.event_type == "apply_queued")
                .count(),
            1
        );
        drop(store); // release the single-writer lock

        let paths = AppPaths::new(dir.path().join("config"), dir.path().to_path_buf());
        execute_package(
            PackageArgs {
                lead: record.lead_id.to_string(),
            },
            &Config::default(),
            &paths,
            false,
        )
        .await
        .unwrap();

        {
            let store = JsonlEventStore::open(dir.path().join("events.jsonl")).unwrap();
            let queued: Vec<_> = store
                .replay()
                .unwrap()
                .into_iter()
                .filter(|e| e.event_type == "apply_queued")
                .collect();
            assert_eq!(queued.len(), 2);
            // The rebuilt package still carries the posting URL.
            assert_eq!(queued[1].payload["url"], record.url.clone().unwrap());
        } // release the single-writer lock before the re-entry below
        // The browser-open intent fired twice: once for the mark's own
        // flow, once for the package re-entry (recorded by the test stub;
        // real runs spawn the browser).
        let expected = record.url.clone().unwrap();
        assert_eq!(opened_urls(), vec![expected.clone(), expected]);

        // The machine-readable path must not spawn a browser: same re-entry
        // with --json fires no open intent (PR #17 review).
        execute_package(
            PackageArgs {
                lead: record.lead_id.to_string(),
            },
            &Config::default(),
            &paths,
            true,
        )
        .await
        .unwrap();
        assert!(opened_urls().is_empty());
    }

    #[tokio::test]
    async fn execute_package_bails_on_unmarked_lead() {
        // Unmarked = unapproved: nothing is appended (the mark, which is
        // the approval, must happen first).
        let dir = tempfile::tempdir().unwrap();
        let (store, record) = ingested_lead(
            &dir,
            &Config::default(),
            ExtractedFields {
                title: Some("Engineer".into()),
                company: Some("Acme".into()),
                ..Default::default()
            },
            "body",
        );
        drop(store);

        let paths = AppPaths::new(dir.path().join("config"), dir.path().to_path_buf());
        assert!(
            execute_package(
                PackageArgs {
                    lead: record.lead_id.to_string()
                },
                &Config::default(),
                &paths,
                false,
            )
            .await
            .is_err()
        );
        let store = JsonlEventStore::open(dir.path().join("events.jsonl")).unwrap();
        assert_eq!(
            store
                .replay()
                .unwrap()
                .iter()
                .filter(|e| e.event_type == "apply_queued")
                .count(),
            0
        );
    }

    #[test]
    fn cheat_sheet_derives_answers_from_resume() {
        let resume = Resume {
            basics: resume::Basics {
                name: Some("Avery Example".into()),
                email: Some("avery@example.com".into()),
                phone: None,
                location: Some(resume::Location {
                    city: Some("San Francisco".into()),
                    region: Some("CA".into()),
                    country_code: Some("US".into()),
                }),
            },
            work: vec![resume::Work {
                name: Some("Acme".into()),
                position: Some("Staff Engineer".into()),
                ..Default::default()
            }],
            ..Default::default()
        };
        let sheet = cheat_sheet(&resume);
        let qa: Vec<(&str, &str)> = sheet
            .iter()
            .map(|e| (e.question.as_str(), e.answer.as_str()))
            .collect();
        assert!(qa.contains(&("Full name", "Avery Example")));
        assert!(qa.contains(&("Email address", "avery@example.com")));
        assert!(qa.contains(&("Location", "San Francisco, CA, US")));
        assert!(qa.contains(&("Current or most recent title", "Staff Engineer")));
        assert!(qa.contains(&("Current or most recent employer", "Acme")));
        // Phone is absent → omitted.
        assert!(!qa.iter().any(|(q, _)| *q == "Phone number"));
    }

    #[test]
    fn prepare_package_assembles_paths_and_cheat_sheet() {
        let dir = tempfile::tempdir().unwrap();
        let resume_path = dir.path().join("resume.json");
        std::fs::write(
            &resume_path,
            r#"{"basics": {"name": "Avery Example"}, "work": [{"name": "Acme", "position": "Staff Engineer"}]}"#,
        )
        .unwrap();
        let config = Config {
            resume_path: Some(resume_path.clone()),
            cover_letter_path: Some(dir.path().join("letter.pdf")),
            ..Default::default()
        };
        let record = lead_record(Some("https://example.com/j"));

        let package = prepare_package(&config, &record).unwrap();
        assert_eq!(package.url.as_deref(), Some("https://example.com/j"));
        assert_eq!(
            package.package.resume_path.as_deref(),
            Some(resume_path.with_extension("pdf").to_str().unwrap())
        );
        assert_eq!(
            package.package.cover_letter_path.as_deref(),
            Some(dir.path().join("letter.pdf").to_str().unwrap())
        );
        assert!(!package.package.cheat_sheet.is_empty());
    }

    #[test]
    fn prepare_package_fails_on_broken_resume() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            resume_path: Some(dir.path().join("missing.json")),
            ..Default::default()
        };
        let record = lead_record(Some("https://example.com/j"));
        assert!(prepare_package(&config, &record).is_err());
    }
}
