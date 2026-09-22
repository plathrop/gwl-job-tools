use std::io::Write;

use crossterm::{
    event::{Event, KeyCode, KeyEvent, KeyModifiers, read},
    terminal::{disable_raw_mode, enable_raw_mode},
};
use miette::{IntoDiagnostic, Result};
use tracing::{debug, instrument, warn};
use uuid::Uuid;

use super::{cheat_sheet, mark_lead, open_workspace};
use crate::{
    cli::Mark,
    config::{AppPaths, Config},
    event_store::EventStore,
    projections::LeadRecord,
    render, resume,
};

#[instrument(skip_all, fields(review.run_id))]
pub async fn execute_review(config: &Config, paths: &AppPaths, color: bool) -> Result<()> {
    // One id per review invocation (GWLJ-u8psvi): every mark made in the
    // session inherits the `review.run_id` field in the log — mark_lead is
    // a plain fn with no span of its own, so its info! events inherit the
    // field from the enclosing review span — so "which leads did I action
    // in the session that crashed at lead 7?" is a log grep away.
    let run_id = Uuid::now_v7();
    tracing::Span::current().record("review.run_id", run_id.to_string());

    let (mut store, projection) = open_workspace(paths)?;
    let pending = projection.pending_queue();
    debug!(pending = pending.len(), %run_id, "review session");

    if pending.is_empty() {
        println!("no pending leads");
        return Ok(());
    }

    // The session line lets the user cross-reference a run in the log file
    // from the terminal (the span field covers the log side).
    println!("review session {run_id}");
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
                    // The final click is the user's (design doc §5); remind
                    // them to record the fact once it's done (design doc
                    // 0002).
                    let prefix: String = record.lead_id.to_string().chars().take(8).collect();
                    println!("  next: record with `gwl-jobs applied {prefix}` once submitted");
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
                    // Post-mark hints (GWLJ-do8pqx + design doc 0002): the JD
                    // text for the manual application, and the command that
                    // records the outcome the flow ends with.
                    let prefix: String = record.lead_id.to_string().chars().take(8).collect();
                    println!(
                        "  next: JD `gwl-jobs show {prefix} --jd`; record `gwl-jobs applied {prefix}` once submitted"
                    );
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
