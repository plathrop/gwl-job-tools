use miette::{IntoDiagnostic, Result, miette};
use tracing::instrument;
use uuid::Uuid;

use super::{open_workspace, replay_lead, select_lead};
use crate::{cli::ShowArgs, config::AppPaths, event_store::EventStore, render};

#[instrument(skip_all)]
pub async fn execute_show(args: ShowArgs, paths: &AppPaths, json: bool, color: bool) -> Result<()> {
    let (store, projection) = open_workspace(paths)?;

    let record = select_lead(&projection, &args.lead)?;
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
pub(crate) fn load_raw_text(store: &impl EventStore, lead_id: Uuid) -> Result<Option<String>> {
    Ok(replay_lead(store, lead_id)?.raw_text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        commands::{record_ingest, test_support::*},
        config::Config,
        projections,
    };

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
