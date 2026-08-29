//! Command implementations. Thin: I/O wiring around the domain.

use std::io::Write;

use crossterm::{
    event::{Event, KeyCode, KeyEvent, KeyModifiers, read},
    terminal::{disable_raw_mode, enable_raw_mode},
};
use jiff::Timestamp;
use miette::{Context, IntoDiagnostic, Result, bail, miette};
use serde::Serialize;
use serde_with::skip_serializing_none;
use tracing::{debug, info, instrument, warn};
use url::Url;
use uuid::Uuid;

use crate::{
    cli::{
        AppliedArgs, EventsArgs, IngestArgs, InterviewedArgs, ListArgs, Mark, MarkArgs,
        OfferedArgs, OutcomeArgs, OutcomeType, ScreenedArgs, ShowArgs,
    },
    config::{AppPaths, Config},
    domain::{
        events::{
            ApplyPackage, ApplyQueuedPayload, CheatSheetEntry, EventEnvelope, ExtractedFields,
            OutcomePayload, PendingEvent, event_type,
        },
        gates::{self, GateFailure},
        identity::{self, compute_identity},
        lead::{self, IngestKind, LeadState},
        scoring::{self, ScoreResult},
    },
    event_store::{EventStore, JsonlEventStore},
    ingest::{self, IngestOutcome},
    projections::{self, LeadRecord, Projection},
    render,
    resume::{self, Resume},
};

const EVENT_LOG_NAME: &str = "events.jsonl";

#[instrument(skip_all)]
pub async fn execute_ingest(
    args: IngestArgs,
    config: &Config,
    paths: &AppPaths,
    json: bool,
    color: bool,
) -> Result<()> {
    // Load the resume before fetching: a configured-but-broken resume fails
    // loudly before any network I/O (decision 0004).
    let resume = resume::load(config.resume_path.as_deref())?;
    let resume_skills = resume
        .as_ref()
        .map(resume::Resume::keywords)
        .unwrap_or_default();

    // Fetch/extract *before* acquiring the single-writer lock: network waits
    // must not hold the lock (durability contract, design doc 0001 §1).
    let mut outcome = match (&args.url, &args.file) {
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
    // The lead source is user-supplied (how the job was found); the adapter
    // is always known (derived from the URL). Default to `unknown`.
    outcome.source = args.source.unwrap_or_default().as_str().to_string();

    // Acquire the lock only for the fast read → decide → append cycle.
    let mut store = JsonlEventStore::open(paths.data_dir().join(EVENT_LOG_NAME))?;
    let events = store.replay()?;
    let projection = projections::rebuild(&events)?;

    let summary = record_ingest(&mut store, &projection, config, &resume_skills, outcome)?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&summary).into_diagnostic()?
        );
    } else {
        // Rebuild the projection to get the updated lead record, then render
        // the card.
        let events = store.replay()?;
        let projection = projections::rebuild(&events)?;
        let record = projection
            .leads
            .get(&summary.lead_id)
            .ok_or_else(|| miette!("lead {} not found after ingest", summary.lead_id))?;
        render::render_card(record, color)?;
    }
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
        // Lead IDs are UUIDv4 (decision 0008): random, so short prefixes
        // stay unambiguous for `<lead>` addressing. Event IDs stay v7
        // (time-ordered for the append-only log).
        None => (Uuid::new_v4(), LeadState::default()),
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
    if let Some(score) = &score {
        debug!(
            composite = score.composite,
            breakdown = %score.breakdown,
            "lead scored"
        );
    }

    let (kind, pending) = lead::decide_ingest(
        &state,
        &identity,
        &outcome.adapter,
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
        adapter: outcome.adapter,
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
    pub adapter: String,
    pub source: String,
    pub url: Option<String>,
    pub extracted: ExtractedFields,
}

#[instrument(skip_all)]
pub async fn execute_show(args: ShowArgs, paths: &AppPaths, json: bool, color: bool) -> Result<()> {
    let store = JsonlEventStore::open(paths.data_dir().join(EVENT_LOG_NAME))?;
    let events = store.replay()?;
    let projection = projections::rebuild(&events)?;

    let record = select_lead(&projection, &args.id)?;
    if args.jd {
        match load_raw_text(&store, record.lead_id)? {
            Some(text) => println!("{text}"),
            None => return Err(miette!("no raw text for lead {}", record.lead_id)),
        }
    } else if json {
        println!(
            "{}",
            serde_json::to_string_pretty(record).into_diagnostic()?
        );
    } else {
        render::render_card(record, color)?;
    }
    Ok(())
}

/// The raw posting text for a lead, from the latest `ingested`/`updated`
/// snapshot in its stream (the projection doesn't carry it).
fn load_raw_text(store: &impl EventStore, lead_id: Uuid) -> Result<Option<String>> {
    let stream = LeadState::stream_id(lead_id);
    let mut state = LeadState::default();
    for event in store.load(&stream)? {
        lead::evolve(&mut state, &event);
    }
    Ok(state.raw_text)
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

/// One row of the `list` output (design doc §8).
#[skip_serializing_none]
#[derive(Clone, Debug, Serialize)]
pub struct QueueEntry {
    pub rank: usize,
    pub lead_id: Uuid,
    pub composite: Option<u64>,
    pub title: Option<String>,
    pub company: Option<String>,
    pub deferral_count: u64,
    pub mark: Option<String>,
    pub outcome: Option<String>,
}

fn queue_entry(rank: usize, record: &LeadRecord) -> QueueEntry {
    QueueEntry {
        rank,
        lead_id: record.lead_id,
        composite: record.latest_score.as_ref().map(|s| s.composite),
        title: record.extracted.title.clone(),
        company: record.extracted.company.clone(),
        deferral_count: record.deferral_count,
        mark: record.latest_mark.clone(),
        outcome: record.latest_outcome.as_ref().map(|o| o.event_type.clone()),
    }
}

#[instrument(skip_all)]
pub async fn execute_list(args: ListArgs, paths: &AppPaths, json: bool, color: bool) -> Result<()> {
    let store = JsonlEventStore::open(paths.data_dir().join(EVENT_LOG_NAME))?;
    let events = store.replay()?;
    let projection = projections::rebuild(&events)?;

    let records: Vec<&LeadRecord> = if args.all {
        let mut leads: Vec<&LeadRecord> = projection.leads.values().collect();
        leads.sort_by(|a, b| {
            let sa = a.latest_score.as_ref().map(|s| s.composite).unwrap_or(0);
            let sb = b.latest_score.as_ref().map(|s| s.composite).unwrap_or(0);
            sb.cmp(&sa).then_with(|| a.first_seen.cmp(&b.first_seen))
        });
        leads
    } else {
        projection.pending_queue()
    };

    if json {
        let entries: Vec<QueueEntry> = records
            .iter()
            .enumerate()
            .map(|(i, r)| queue_entry(i + 1, r))
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&entries).into_diagnostic()?
        );
    } else {
        render::render_list(&records, color)?;
    }
    Ok(())
}

#[instrument(skip_all)]
pub async fn execute_mark(args: MarkArgs, config: &Config, paths: &AppPaths) -> Result<()> {
    let mut store = JsonlEventStore::open(paths.data_dir().join(EVENT_LOG_NAME))?;
    let events = store.replay()?;
    let projection = projections::rebuild(&events)?;

    let record = select_lead(&projection, &args.lead)?;
    let lead_id = record.lead_id;
    let apply = mark_lead(&mut store, config, record, args.mark, args.note)?;

    let mut output = serde_json::json!({
        "lead_id": lead_id,
        "mark": args.mark.as_str(),
    });
    // Surface the prepared package (including the cheat sheet) so the user
    // can see the answers while completing the opened form.
    if let Some(package) = &apply {
        output["package"] = serde_json::to_value(package).into_diagnostic()?;
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&output).into_diagnostic()?
    );
    Ok(())
}

/// Mark a lead: prepare the package (for apply-automatically), decide the
/// events, append them, and open the URL. Returns the prepared package so the
/// caller can surface the cheat sheet.
fn mark_lead(
    store: &mut impl EventStore,
    config: &Config,
    record: &LeadRecord,
    mark: Mark,
    note: Option<String>,
) -> Result<Option<ApplyQueuedPayload>> {
    let lead_id = record.lead_id;
    // Prepare the apply package for apply-automatically (the mark IS the
    // approval; the package is assembled before the batch append so a
    // preparation failure leaves the lead unmarked).
    let apply = if mark == Mark::ApplyAutomatically {
        Some(prepare_package(config, record)?)
    } else {
        None
    };

    let pending = lead::decide_mark(mark.as_str(), note, apply.clone())?;

    let stream = LeadState::stream_id(lead_id);
    let mut state = LeadState::default();
    for event in store.load(&stream)? {
        lead::evolve(&mut state, &event);
    }
    store.append(&stream, state.seq, &pending, Uuid::now_v7())?;
    info!(%lead_id, mark = %mark.as_str(), "lead marked");

    // Open the posting URL for apply-automatically (the final click is the
    // user's; v0 just opens the page). Best-effort: a launch failure must
    // not report the mark as failed (the events are already durably
    // appended).
    if mark == Mark::ApplyAutomatically
        && let Some(url) = record.url.as_deref()
    {
        open_url(url);
    }
    Ok(apply)
}

#[instrument(skip_all, fields(color = %color))]
pub async fn execute_review(config: &Config, paths: &AppPaths, color: bool) -> Result<()> {
    let mut store = JsonlEventStore::open(paths.data_dir().join(EVENT_LOG_NAME))?;
    let events = store.replay()?;
    let projection = projections::rebuild(&events)?;
    let pending = projection.pending_queue();
    debug!(pending = pending.len(), "review session");

    if pending.is_empty() {
        println!("no pending leads");
        return Ok(());
    }

    review_loop(&mut store, config, &pending, color)
}

/// A guard that enables raw mode on construction and restores it on drop
/// (panic-safe, and a restore failure never masks the loop's own error).
struct RawModeGuard;

impl RawModeGuard {
    fn new() -> Result<Self> {
        enable_raw_mode().into_diagnostic()?;
        Ok(Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        if let Err(err) = disable_raw_mode() {
            warn!(error = %err, "failed to restore terminal raw mode");
        }
    }
}

/// Read a single key, with raw mode scoped to the read (so card/prompt
/// rendering happens in normal mode, where `\n` returns the cursor to column
/// 0). Returns `None` for Ctrl-C.
fn read_review_key() -> Result<Option<char>> {
    let _guard = RawModeGuard::new()?;
    loop {
        match read().into_diagnostic()? {
            Event::Key(KeyEvent {
                code: KeyCode::Char(c),
                modifiers,
                ..
            }) => {
                if modifiers.contains(KeyModifiers::CONTROL) && c == 'c' {
                    return Ok(None);
                }
                return Ok(Some(c));
            }
            _ => continue,
        }
    }
}

/// The interactive review loop (design doc 0001 §5): print the ranked queue,
/// then step through pending leads highest-score-first, render each card, and
/// take a single-key mark. Unknown keys re-prompt (they do not skip).
fn review_loop(
    store: &mut impl EventStore,
    config: &Config,
    pending: &[&LeadRecord],
    color: bool,
) -> Result<()> {
    // Print the ranked queue first (design doc §5).
    render::render_list(pending, color)?;
    println!();

    for record in pending {
        render::render_card(record, color)?;

        // Re-prompt until a recognized key (or Ctrl-C) is read.
        loop {
            print!("{} ", render::render_prompt(color));
            std::io::stdout().flush().into_diagnostic()?;

            let Some(key) = read_review_key()? else {
                return Ok(()); // Ctrl-C quits.
            };
            println!();

            match key {
                'a' => {
                    let apply = mark_lead(store, config, record, Mark::ApplyAutomatically, None)?;
                    if let Some(package) = &apply {
                        print!(
                            "{}",
                            render::render_cheat_sheet(&package.package.cheat_sheet)
                        );
                        std::io::stdout().flush().into_diagnostic()?;
                    }
                    break;
                }
                'm' => {
                    // apply-manual: the user takes personal action. Mark
                    // first, then provide the cheat sheet + URL best-effort
                    // (a broken resume must not abort the session over an
                    // accessory cheat sheet).
                    mark_lead(store, config, record, Mark::ApplyManual, None)?;
                    match resume::load(config.resume_path.as_deref()) {
                        Ok(resume) => {
                            let sheet = resume.as_ref().map(cheat_sheet).unwrap_or_default();
                            print!("{}", render::render_cheat_sheet(&sheet));
                        }
                        Err(err) => warn!(error = %err, "could not load resume for cheat sheet"),
                    }
                    if let Some(url) = record.url.as_deref() {
                        println!("  URL: {}", render::sanitize(url));
                    }
                    std::io::stdout().flush().into_diagnostic()?;
                    break;
                }
                'd' => {
                    mark_lead(store, config, record, Mark::Defer, None)?;
                    break;
                }
                'i' => {
                    mark_lead(store, config, record, Mark::Ignore, None)?;
                    break;
                }
                's' => {
                    debug!(lead_id = %record.lead_id, "skipped lead (session-local)");
                    break;
                }
                'q' => return Ok(()),
                _ => {
                    // Unknown key: re-prompt (do not skip the lead).
                    debug!(key = %key, lead_id = %record.lead_id, "unrecognized review key");
                }
            }
        }
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
fn prepare_package(config: &Config, record: &LeadRecord) -> Result<ApplyQueuedPayload> {
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
fn cheat_sheet(resume: &Resume) -> Vec<CheatSheetEntry> {
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

/// Open a URL in the user's default browser (best-effort; the final click is
/// the user's). A launch failure is logged and surfaced on stderr, but does
/// not fail the command — the mark has already been durably recorded.
fn open_url(url: &str) {
    let mut cmd = if cfg!(target_os = "macos") {
        let mut c = std::process::Command::new("open");
        c.arg(url);
        c
    } else if cfg!(target_os = "windows") {
        // `rundll32 url.dll,FileProtocolHandler` opens the URL without a
        // shell, so metacharacters in the URL are not interpreted (unlike
        // `cmd /C start`).
        let mut c = std::process::Command::new("rundll32");
        c.args(["url.dll,FileProtocolHandler", url]);
        c
    } else {
        let mut c = std::process::Command::new("xdg-open");
        c.arg(url);
        c
    };
    if let Err(err) = cmd.spawn() {
        warn!(error = %err, "failed to open posting URL in browser");
        eprintln!("note: could not open {url} in a browser — open it manually");
    }
}

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

/// Resolve a lead by prefix and append one user-recorded outcome event to its
/// stream. Returns the lead id.
fn record_outcome(
    store: &mut impl EventStore,
    projection: &Projection,
    prefix: &str,
    event_type: &'static str,
    payload: OutcomePayload,
    occurred_at: Option<Timestamp>,
) -> Result<Uuid> {
    let lead_id = select_lead(projection, prefix)?.lead_id;
    let stream = LeadState::stream_id(lead_id);
    let mut state = LeadState::default();
    for event in store.load(&stream)? {
        lead::evolve(&mut state, &event);
    }
    let mut pending = PendingEvent::new(event_type, None, &payload)?;
    pending.occurred_at = occurred_at;
    store.append(&stream, state.seq, &[pending], Uuid::now_v7())?;
    info!(%lead_id, event_type, "outcome recorded");
    Ok(lead_id)
}

#[instrument(skip_all)]
pub async fn execute_applied(args: AppliedArgs, paths: &AppPaths) -> Result<()> {
    let mut store = JsonlEventStore::open(paths.data_dir().join(EVENT_LOG_NAME))?;
    let projection = projections::rebuild(&store.replay()?)?;
    let occurred_at = args.at.as_deref().map(parse_occurred_at).transpose()?;
    let lead_id = record_outcome(
        &mut store,
        &projection,
        &args.lead,
        event_type::APPLIED,
        OutcomePayload {
            method: args.method.map(|m| m.as_str().to_string()),
            note: args.note,
            ..Default::default()
        },
        occurred_at,
    )?;
    println!("{lead_id}");
    Ok(())
}

#[instrument(skip_all)]
pub async fn execute_screened(args: ScreenedArgs, paths: &AppPaths) -> Result<()> {
    let mut store = JsonlEventStore::open(paths.data_dir().join(EVENT_LOG_NAME))?;
    let projection = projections::rebuild(&store.replay()?)?;
    let occurred_at = args.at.as_deref().map(parse_occurred_at).transpose()?;
    let lead_id = record_outcome(
        &mut store,
        &projection,
        &args.lead,
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
    let mut store = JsonlEventStore::open(paths.data_dir().join(EVENT_LOG_NAME))?;
    let projection = projections::rebuild(&store.replay()?)?;
    let occurred_at = args.at.as_deref().map(parse_occurred_at).transpose()?;
    let lead_id = record_outcome(
        &mut store,
        &projection,
        &args.lead,
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
    let mut store = JsonlEventStore::open(paths.data_dir().join(EVENT_LOG_NAME))?;
    let projection = projections::rebuild(&store.replay()?)?;
    let occurred_at = args.at.as_deref().map(parse_occurred_at).transpose()?;
    let lead_id = record_outcome(
        &mut store,
        &projection,
        &args.lead,
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
    let mut store = JsonlEventStore::open(paths.data_dir().join(EVENT_LOG_NAME))?;
    let projection = projections::rebuild(&store.replay()?)?;
    let occurred_at = args.at.as_deref().map(parse_occurred_at).transpose()?;
    let lead_id = record_outcome(
        &mut store,
        &projection,
        &args.lead,
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

#[instrument(skip_all)]
pub async fn execute_events(args: EventsArgs, paths: &AppPaths) -> Result<()> {
    let store = JsonlEventStore::open(paths.data_dir().join(EVENT_LOG_NAME))?;
    let events = store.replay()?;
    // Resolve the lead prefix once (unambiguous, like the other lead-addressed
    // commands) so a short/empty prefix can't silently mix unrelated leads.
    let lead_id = match &args.lead {
        Some(prefix) => {
            let projection = projections::rebuild(&events)?;
            Some(select_lead(&projection, prefix)?.lead_id)
        }
        None => None,
    };
    for event in filter_events(&events, lead_id, args.event_type.as_deref()) {
        println!("{}", serde_json::to_string(event).into_diagnostic()?);
    }
    Ok(())
}

/// Filter events by lead id and/or event type.
fn filter_events<'a>(
    events: &'a [EventEnvelope],
    lead_id: Option<Uuid>,
    event_type: Option<&str>,
) -> Vec<&'a EventEnvelope> {
    events
        .iter()
        .filter(|e| {
            let lead_ok = match lead_id {
                Some(id) => lead::stream_lead_id(&e.stream) == Some(id),
                None => true,
            };
            let type_ok = match event_type {
                Some(t) => e.event_type == *t,
                None => true,
            };
            lead_ok && type_ok
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(raw_text: &str) -> IngestOutcome {
        IngestOutcome {
            adapter: "drop-in".into(),
            source: "unknown".into(),
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
    fn lead_source_flows_into_ingested_payload() {
        // The lead source (--source) is user-supplied and distinct from the
        // extraction adapter. A non-default source must land in the event.
        let dir = tempfile::tempdir().unwrap();
        let (mut store, projection) = store_and_projection(&dir);
        let mut outcome = outcome("body");
        outcome.url = Some("https://example.com/job/12".into());
        outcome.extracted.title = Some("Staff Engineer".into());
        outcome.extracted.company = Some("Acme".into());
        outcome.source = "recruiter".into();

        record_ingest(&mut store, &projection, &Config::default(), &[], outcome).unwrap();

        let events = store.replay().unwrap();
        assert_eq!(events[0].payload["adapter"], "drop-in");
        assert_eq!(events[0].payload["source"], "recruiter");
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

        let projection = projections::rebuild(&store.replay().unwrap()).unwrap();
        let lead_id = record_outcome(
            &mut store,
            &projection,
            &summary.lead_id.to_string(),
            event_type::APPLIED,
            OutcomePayload {
                method: Some("manual".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        assert_eq!(lead_id, summary.lead_id);

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

        let projection = projections::rebuild(&store.replay().unwrap()).unwrap();
        let at = "2026-08-15T00:00:00Z".parse::<Timestamp>().unwrap();
        record_outcome(
            &mut store,
            &projection,
            &summary.lead_id.to_string(),
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

        let projection = projections::rebuild(&store.replay().unwrap()).unwrap();
        record_outcome(
            &mut store,
            &projection,
            &summary.lead_id.to_string(),
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

    #[tokio::test]
    async fn events_lead_resolves_prefix_with_errors() {
        // Copilot: `--lead` is documented as an unambiguous UUID prefix;
        // unlike the other lead-addressed commands the filter silently
        // emits all matching streams and silently succeeds on no match.
        // The prefix must resolve once, with the zero/multiple-match errors.
        let dir = tempfile::tempdir().unwrap();
        let mut store = JsonlEventStore::open(dir.path().join(EVENT_LOG_NAME)).unwrap();
        let mut projection = projections::rebuild(&[]).unwrap();
        for _ in 0..2 {
            let mut o = outcome("body");
            o.url = Some(format!("https://example.com/{}", Uuid::now_v7()));
            record_ingest(&mut store, &projection, &Config::default(), &[], o).unwrap();
            projection = projections::rebuild(&store.replay().unwrap()).unwrap();
        }
        drop(store); // release the single-writer lock for execute_events
        let paths = AppPaths::new(dir.path().join("config"), dir.path().to_path_buf());

        // Ambiguous: the empty prefix matches both leads.
        assert!(
            execute_events(
                EventsArgs {
                    lead: Some("".into()),
                    event_type: None
                },
                &paths,
            )
            .await
            .is_err()
        );
        // No match: 'z' cannot prefix any UUID.
        assert!(
            execute_events(
                EventsArgs {
                    lead: Some("z".into()),
                    event_type: None
                },
                &paths,
            )
            .await
            .is_err()
        );
    }

    // ── apply package (Increment 4a) ──────────────────────────────

    fn lead_record(url: Option<&str>) -> LeadRecord {
        LeadRecord {
            lead_id: Uuid::now_v7(),
            dedupe_key: None,
            identifiers: crate::domain::events::Identifiers::default(),
            adapter: None,
            source: None,
            url: url.map(Into::into),
            extracted: ExtractedFields::default(),
            latest_mark: None,
            deferral_count: 0,
            apply_queued: false,
            latest_rejection: None,
            latest_score: None,
            latest_outcome: None,
            event_count: 0,
            first_seen: Timestamp::now(),
            last_event: Timestamp::now(),
        }
    }

    #[test]
    fn cheat_sheet_derives_answers_from_resume() {
        let resume = Resume {
            basics: resume::Basics {
                name: Some("Grey".into()),
                email: Some("grey@example.com".into()),
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
        assert!(qa.contains(&("Full name", "Grey")));
        assert!(qa.contains(&("Email address", "grey@example.com")));
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
            r#"{"basics": {"name": "Grey"}, "work": [{"name": "Acme", "position": "Staff Engineer"}]}"#,
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

    #[test]
    fn mark_lead_appends_reviewed() {
        let dir = tempfile::tempdir().unwrap();
        let (mut store, projection) = store_and_projection(&dir);
        let mut outcome = outcome("body");
        outcome.url = Some("https://example.com/job/13".into());
        outcome.extracted.title = Some("Staff Engineer".into());
        outcome.extracted.company = Some("Acme".into());
        record_ingest(&mut store, &projection, &Config::default(), &[], outcome).unwrap();

        // Rebuild the projection to get the lead record, then mark it defer.
        let events = store.replay().unwrap();
        let projection = projections::rebuild(&events).unwrap();
        let record = projection.leads.values().next().unwrap();
        mark_lead(&mut store, &Config::default(), record, Mark::Defer, None).unwrap();

        let events = store.replay().unwrap();
        let last = events.last().unwrap();
        assert_eq!(last.event_type, "reviewed");
        assert_eq!(last.payload["mark"], "defer");
    }

    #[test]
    fn load_raw_text_returns_latest_snapshot_text() {
        let dir = tempfile::tempdir().unwrap();
        let (mut store, projection) = store_and_projection(&dir);
        let mut first = outcome("the full JD text v1");
        first.url = Some("https://example.com/job/14".into());
        first.extracted.title = Some("Staff Engineer".into());
        first.extracted.company = Some("Acme".into());
        record_ingest(&mut store, &projection, &Config::default(), &[], first).unwrap();

        // Re-ingest the same lead with different text (the `updated` path);
        // the latest snapshot must win, not the first.
        let events = store.replay().unwrap();
        let projection = projections::rebuild(&events).unwrap();
        let lead_id = projection.leads.values().next().unwrap().lead_id;
        let mut second = outcome("the full JD text v2");
        second.url = Some("https://example.com/job/14".into());
        second.extracted.title = Some("Staff Engineer".into());
        second.extracted.company = Some("Acme".into());
        record_ingest(&mut store, &projection, &Config::default(), &[], second).unwrap();

        assert_eq!(
            load_raw_text(&store, lead_id).unwrap().as_deref(),
            Some("the full JD text v2")
        );
    }
}
