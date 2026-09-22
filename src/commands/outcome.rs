use jiff::Timestamp;
use miette::{Context, IntoDiagnostic, Result, bail};
use tracing::{info, instrument};
use uuid::Uuid;

use super::{open_workspace, replay_lead, select_lead};
use crate::{
    cli::{
        AppliedArgs, ApplyMethod, InterviewedArgs, OfferedArgs, OutcomeArgs, OutcomeType,
        ScreenedArgs,
    },
    config::AppPaths,
    domain::{
        events::{OutcomePayload, PendingEvent, event_type},
        lead::LeadState,
    },
    event_store::EventStore,
    projections::LeadRecord,
};

/// Parse the `--at` flag: an RFC 3339 timestamp (e.g. 2026-08-15T00:00:00Z)
/// or a bare date (YYYY-MM-DD, defaulting to noon UTC).
fn parse_occurred_at(s: &str) -> Result<Timestamp> {
    if let Ok(ts) = s.parse::<Timestamp>() {
        return Ok(ts);
    }
    let date: jiff::civil::Date = s
        .parse()
        .into_diagnostic()
        .wrap_err_with(|| format!("parsing --at '{s}' (expected RFC 3339 or YYYY-MM-DD)"))?;
    let zoned = date
        .at(12, 0, 0, 0)
        .to_zoned(jiff::tz::TimeZone::UTC)
        .into_diagnostic()?;
    Ok(zoned.timestamp())
}

/// Append one user-recorded outcome event to `lead_id`'s stream. The caller
/// resolves the `<lead>` argument once, so a bad prefix is reported by the
/// command that owns the argument rather than re-resolved here.
fn record_outcome(
    store: &mut impl EventStore,
    lead_id: Uuid,
    event_type: &'static str,
    payload: OutcomePayload,
    occurred_at: Option<Timestamp>,
) -> Result<()> {
    let stream = LeadState::stream_id(lead_id);
    let state = replay_lead(store, lead_id)?;
    let mut pending = PendingEvent::new(event_type, None, &payload)?;
    pending.occurred_at = occurred_at;
    store.append(&stream, state.seq, &[pending], Uuid::now_v7())?;
    info!(%lead_id, event_type, "outcome recorded");
    Ok(())
}

#[instrument(skip_all)]
pub async fn execute_applied(args: AppliedArgs, paths: &AppPaths) -> Result<()> {
    let (mut store, projection) = open_workspace(paths)?;
    let occurred_at = args.at.as_deref().map(parse_occurred_at).transpose()?;
    // The method defaults from the lead's apply mark (design doc 0002): the
    // mark recorded which flow was chosen, so `gwl-jobs applied <lead>` is
    // enough after a review mark. An explicit --method still wins.
    let record = select_lead(&projection, &args.lead)?;
    let method = resolve_apply_method(record, args.method);
    record_outcome(
        &mut store,
        record.lead_id,
        event_type::APPLIED,
        OutcomePayload {
            method,
            note: args.note,
            ..Default::default()
        },
        occurred_at,
    )?;
    println!("{}", record.lead_id);
    Ok(())
}

/// The `applied` submission method, resolved as `--method` if given, else
/// the method the lead's apply mark implies (design doc 0002). Delegates
/// to `LeadRecord::mark_method` so the mark→method mapping exists in
/// exactly one place (PR #16 review).
fn resolve_apply_method(record: &LeadRecord, method: Option<ApplyMethod>) -> Option<String> {
    method
        .map(|m| m.as_str().to_string())
        .or_else(|| record.mark_method().map(str::to_string))
}

#[instrument(skip_all)]
pub async fn execute_screened(args: ScreenedArgs, paths: &AppPaths) -> Result<()> {
    let (mut store, projection) = open_workspace(paths)?;
    let occurred_at = args.at.as_deref().map(parse_occurred_at).transpose()?;
    let lead_id = select_lead(&projection, &args.lead)?.lead_id;
    record_outcome(
        &mut store,
        lead_id,
        event_type::SCREENED,
        OutcomePayload {
            contact: args.contact,
            note: args.note,
            ..Default::default()
        },
        occurred_at,
    )?;
    println!("{lead_id}");
    Ok(())
}

#[instrument(skip_all)]
pub async fn execute_interviewed(args: InterviewedArgs, paths: &AppPaths) -> Result<()> {
    let (mut store, projection) = open_workspace(paths)?;
    let occurred_at = args.at.as_deref().map(parse_occurred_at).transpose()?;
    let lead_id = select_lead(&projection, &args.lead)?.lead_id;
    record_outcome(
        &mut store,
        lead_id,
        event_type::INTERVIEWED,
        OutcomePayload {
            stage: args.stage,
            note: args.note,
            ..Default::default()
        },
        occurred_at,
    )?;
    println!("{lead_id}");
    Ok(())
}

#[instrument(skip_all)]
pub async fn execute_offered(args: OfferedArgs, paths: &AppPaths) -> Result<()> {
    let (mut store, projection) = open_workspace(paths)?;
    let occurred_at = args.at.as_deref().map(parse_occurred_at).transpose()?;
    let lead_id = select_lead(&projection, &args.lead)?.lead_id;
    record_outcome(
        &mut store,
        lead_id,
        event_type::OFFERED,
        OutcomePayload {
            note: args.note,
            ..Default::default()
        },
        occurred_at,
    )?;
    println!("{lead_id}");
    Ok(())
}

#[instrument(skip_all)]
pub async fn execute_outcome(args: OutcomeArgs, paths: &AppPaths) -> Result<()> {
    if args.start_date.is_some() && args.outcome != OutcomeType::Accepted {
        bail!("--start-date is only valid for 'accepted'");
    }
    if args.reason.is_some() && args.outcome != OutcomeType::Archived {
        bail!("--reason is only valid for 'archived'");
    }
    let (mut store, projection) = open_workspace(paths)?;
    let occurred_at = args.at.as_deref().map(parse_occurred_at).transpose()?;
    let lead_id = select_lead(&projection, &args.lead)?.lead_id;
    record_outcome(
        &mut store,
        lead_id,
        args.outcome.as_str(),
        OutcomePayload {
            note: args.note,
            start_date: args.start_date,
            reason: args.reason,
            ..Default::default()
        },
        occurred_at,
    )?;
    println!("{lead_id}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        cli::Mark,
        commands::{mark_lead, record_ingest, test_support::*},
        config::Config,
        domain::events::ExtractedFields,
        event_store::JsonlEventStore,
        projections,
    };

    // ── outcome recording (state machine) ─────────────────────────

    #[test]
    fn record_outcome_appends_event() {
        let dir = tempfile::tempdir().unwrap();
        let (mut store, projection) = store_and_projection(&dir);
        let mut outcome = outcome("body");
        outcome.url = Some("https://example.com/job/12".into());
        outcome.extracted.title = Some("Staff Engineer".into());
        outcome.extracted.company = Some("Acme".into());
        let summary =
            record_ingest(&mut store, &projection, &Config::default(), &[], outcome).unwrap();

        record_outcome(
            &mut store,
            summary.lead_id,
            event_type::APPLIED,
            OutcomePayload {
                method: Some("manual".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();

        let events = store.replay().unwrap();
        assert_eq!(events.len(), 3);
        assert_eq!(events[2].event_type, "applied");
        assert_eq!(events[2].payload["method"], "manual");
    }

    #[test]
    fn record_outcome_retro_dates_occurred_at() {
        let dir = tempfile::tempdir().unwrap();
        let (mut store, projection) = store_and_projection(&dir);
        let mut outcome = outcome("body");
        outcome.url = Some("https://example.com/job/13".into());
        outcome.extracted.title = Some("Staff Engineer".into());
        outcome.extracted.company = Some("Acme".into());
        let summary =
            record_ingest(&mut store, &projection, &Config::default(), &[], outcome).unwrap();

        let at = "2026-08-15T00:00:00Z".parse::<Timestamp>().unwrap();
        record_outcome(
            &mut store,
            summary.lead_id,
            event_type::APPLIED,
            OutcomePayload::default(),
            Some(at),
        )
        .unwrap();

        let events = store.replay().unwrap();
        assert_eq!(events[2].occurred_at, at);
    }

    #[test]
    fn parse_occurred_at_parses_rfc3339() {
        let ts = parse_occurred_at("2026-08-15T00:00:00Z").unwrap();
        assert_eq!(ts, "2026-08-15T00:00:00Z".parse::<Timestamp>().unwrap());
    }

    #[test]
    fn parse_occurred_at_rejects_garbage() {
        assert!(parse_occurred_at("not a timestamp").is_err());
    }

    #[test]
    fn parse_occurred_at_accepts_date_only_at_noon_utc() {
        // Retro-recording is a human workflow; humans think in dates.
        // A bare date means noon UTC (documented default).
        let ts = parse_occurred_at("2026-08-01").unwrap();
        assert_eq!(ts, "2026-08-01T12:00:00Z".parse::<Timestamp>().unwrap());
    }

    // ── applied-method defaulting (design doc 0002) ───────────

    #[test]
    fn resolve_apply_method_prefers_explicit_flag() {
        let mut record = lead_record(None);
        record.latest_mark = Some("apply-manual".into());
        // Explicit --method wins over the mark's implication.
        let method = resolve_apply_method(&record, Some(ApplyMethod::AutoAssisted));
        assert_eq!(method.as_deref(), Some("auto-assisted"));
    }

    #[test]
    fn resolve_apply_method_derives_from_mark() {
        let mut record = lead_record(None);
        record.latest_mark = Some("apply-manual".into());
        assert_eq!(
            resolve_apply_method(&record, None).as_deref(),
            Some("manual")
        );
        record.latest_mark = Some("apply-automatically".into());
        assert_eq!(
            resolve_apply_method(&record, None).as_deref(),
            Some("auto-assisted")
        );
        record.latest_mark = Some("defer".into());
        assert_eq!(resolve_apply_method(&record, None), None);
        record.latest_mark = None;
        assert_eq!(resolve_apply_method(&record, None), None);
    }

    #[tokio::test]
    async fn execute_applied_defaults_method_from_mark() {
        // The workflow fix (design doc 0002): after review's `m` key,
        // `gwl-jobs applied <lead>` alone is enough — the apply-manual mark
        // already recorded the flow.
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
        mark_lead(
            &mut store,
            &Config::default(),
            &record,
            Mark::ApplyManual,
            None,
        )
        .unwrap();
        drop(store); // release the single-writer lock

        let paths = AppPaths::new(dir.path().join("config"), dir.path().to_path_buf());
        execute_applied(
            AppliedArgs {
                lead: record.lead_id.to_string(),
                method: None,
                note: None,
                at: None,
            },
            &paths,
        )
        .await
        .unwrap();

        let store = JsonlEventStore::open(dir.path().join("events.jsonl")).unwrap();
        let applied = store
            .replay()
            .unwrap()
            .into_iter()
            .find(|e| e.event_type == "applied")
            .unwrap();
        assert_eq!(applied.payload["method"], "manual");

        // And the derived status reflects the recorded fact.
        let projection = projections::rebuild(&store.replay().unwrap()).unwrap();
        assert_eq!(
            projection
                .leads
                .get(&record.lead_id)
                .unwrap()
                .lifecycle_status(),
            "applied (manual)"
        );
    }

    #[test]
    fn record_outcome_terminal_records_note() {
        let dir = tempfile::tempdir().unwrap();
        let (mut store, projection) = store_and_projection(&dir);
        let mut outcome = outcome("body");
        outcome.url = Some("https://example.com/job/14".into());
        outcome.extracted.title = Some("Staff Engineer".into());
        outcome.extracted.company = Some("Acme".into());
        let summary =
            record_ingest(&mut store, &projection, &Config::default(), &[], outcome).unwrap();

        record_outcome(
            &mut store,
            summary.lead_id,
            event_type::ARCHIVED,
            OutcomePayload {
                note: Some("dead req".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();

        let events = store.replay().unwrap();
        assert_eq!(events[2].event_type, "archived");
        assert_eq!(events[2].payload["note"], "dead req");
    }
}
