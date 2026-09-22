//! Command implementations. Thin: I/O wiring around the domain.
//!
//! One module per command (family) per design doc 0001 §9; `mod.rs` holds
//! the shared read-side helpers and the browser-launch seam, and re-exports
//! the command entry points for `cli::execute`.

mod completion;
mod edit;
mod events;
mod ingest;
mod list;
mod mark;
mod outcome;
mod package;
mod review;
mod show;

#[cfg(test)]
mod test_support;

// Public API: the command entry points `cli::execute` dispatches to.
pub use completion::execute_completion;
pub use edit::execute_edit;
pub use events::execute_events;
pub use ingest::execute_ingest;
#[cfg(test)]
pub(crate) use ingest::record_ingest;
pub use list::execute_list;
pub use mark::execute_mark;
// Crate-visible seams shared across command modules.
pub(crate) use mark::mark_lead;
use miette::{Result, miette};
pub use outcome::{
    execute_applied, execute_interviewed, execute_offered, execute_outcome, execute_screened,
};
pub use package::execute_package;
pub(crate) use package::{cheat_sheet, prepare_package};
pub use review::execute_review;
pub use show::execute_show;
#[cfg(test)]
pub(crate) use show::load_raw_text;
#[cfg(not(test))]
use tracing::warn;
use uuid::Uuid;

use crate::{
    config::AppPaths,
    domain::lead::{self, LeadState},
    event_store::{EventStore, JsonlEventStore},
    projections::{self, LeadRecord, Projection},
};

/// Open the event log and rebuild the projection — the read side every
/// lead-addressed command shares (design doc 0001 §1). Returns the open
/// store (bind it `mut` to append) and the freshly rebuilt projection.
/// `execute_events` is the lone exception: it needs the raw events, not a
/// projection, so it opens the store directly.
fn open_workspace(paths: &AppPaths) -> Result<(JsonlEventStore, Projection)> {
    let store = JsonlEventStore::open(paths.event_log())?;
    let events = store.replay()?;
    let projection = projections::rebuild(&events)?;
    Ok((store, projection))
}

/// Replay a lead's stream into its aggregate state (the load side of the
/// decide/evolve seam; `state.seq` is the `expected_seq` for the next
/// append).
fn replay_lead(store: &impl EventStore, lead_id: Uuid) -> Result<LeadState> {
    let stream = LeadState::stream_id(lead_id);
    let mut state = LeadState::default();
    for event in store.load(&stream)? {
        lead::evolve(&mut state, &event)?;
    }
    Ok(state)
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

/// Open a URL in the user's default browser (best-effort; the final click is
/// the user's). A launch failure is logged and surfaced on stderr, but does
/// not fail the command — the mark has already been durably recorded.
///
/// Browser launching is a real-world side effect with no place in the test
/// harness: under `cfg(test)` this is a no-op, so command-level tests can
/// exercise the full mark/package flows without spawning windows.
#[cfg(not(test))]
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

/// The test-harness counterpart of `open_url`: recording the call instead of
/// spawning a browser, so tests can assert the side effect was attempted.
#[cfg(test)]
fn open_url(url: &str) {
    OPENED_URLS.with(|urls| urls.borrow_mut().push(url.to_string()));
}

/// URLs recorded by the cfg(test) `open_url` stub.
#[cfg(test)]
fn opened_urls() -> Vec<String> {
    OPENED_URLS.with(|urls| std::mem::take(&mut *urls.borrow_mut()))
}

thread_local! {
    /// URLs passed to `open_url` under test, for asserting browser-open
    /// intents without spawning real windows.
    static OPENED_URLS: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::test_support::*;

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
}
