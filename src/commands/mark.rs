use miette::{IntoDiagnostic, Result};
use tracing::{info, instrument};
use uuid::Uuid;

use super::{open_url, open_workspace, prepare_package, replay_lead, select_lead};
use crate::{
    cli::{Mark, MarkArgs},
    config::{AppPaths, Config},
    domain::{
        events::ApplyQueuedPayload,
        lead::{self, LeadState},
    },
    event_store::EventStore,
    projections::LeadRecord,
};

#[instrument(skip_all)]
pub async fn execute_mark(
    args: MarkArgs,
    config: &Config,
    paths: &AppPaths,
    json: bool,
) -> Result<()> {
    let (mut store, projection) = open_workspace(paths)?;

    let record = select_lead(&projection, &args.lead)?;
    let lead_id = record.lead_id;
    let apply = mark_lead(&mut store, config, record, args.mark, args.note)?;

    if json {
        let mut output = serde_json::json!({
            "lead_id": lead_id,
            "mark": args.mark.as_str(),
        });
        // Surface the prepared package (including the cheat sheet) so the
        // user can see the answers while completing the opened form.
        if let Some(package) = &apply {
            output["package"] = serde_json::to_value(package).into_diagnostic()?;
        }
        println!(
            "{}",
            serde_json::to_string_pretty(&output).into_diagnostic()?
        );
    } else {
        // One-line confirmation (design doc §8): `marked <prefix> <mark>`,
        // where the prefix is the 8-char addressing handle (decision 0008).
        let prefix: String = lead_id.to_string().chars().take(8).collect();
        println!("marked {prefix} {}", args.mark.as_str());
    }
    Ok(())
}

/// Mark a lead: prepare the package (for apply-automatically), decide the
/// events, append them, and open the URL. Returns the prepared package so the
/// caller can surface the cheat sheet.
#[instrument(skip_all, fields(mark, remote_known, comp_known))]
pub(crate) fn mark_lead(
    store: &mut impl EventStore,
    config: &Config,
    record: &LeadRecord,
    mark: Mark,
    note: Option<String>,
) -> Result<Option<ApplyQueuedPayload>> {
    let lead_id = record.lead_id;
    // Decision telemetry (theirstack-paid-feed): the mark span carries the
    // mark plus the lead's ingest-time status, so Honeycomb can correlate the
    // operator's action with whether the lead's remote/comp were known.
    tracing::Span::current().record("mark", mark.as_str());
    tracing::Span::current().record("remote_known", record.extracted.remote.is_some());
    tracing::Span::current().record("comp_known", record.extracted.comp.is_some());

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
    let state = replay_lead(store, lead_id)?;
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

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use tracing_subscriber::layer::SubscriberExt;

    use super::*;
    use crate::{
        commands::{record_ingest, test_support::*},
        projections,
    };

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

    // ── decision telemetry: the mark span carries status ────────

    /// A minimal tracing `Layer` that captures every recorded span field as
    /// `(name, debug-value)`, so tests can assert what a span carried.
    struct CapturingLayer {
        fields: Arc<Mutex<Vec<(String, String)>>>,
    }

    struct CaptureVisitor<'a> {
        out: &'a mut Vec<(String, String)>,
    }

    impl tracing::field::Visit for CaptureVisitor<'_> {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            self.out
                .push((field.name().to_string(), format!("{value:?}")));
        }
    }

    impl<S: tracing::Subscriber> tracing_subscriber::layer::Layer<S> for CapturingLayer {
        fn on_record(
            &self,
            _id: &tracing::span::Id,
            values: &tracing::span::Record<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let mut out = Vec::new();
            let mut visitor = CaptureVisitor { out: &mut out };
            values.record(&mut visitor);
            self.fields.lock().unwrap().extend(out);
        }
    }

    #[test]
    fn mark_span_carries_mark_and_ingest_status() {
        let dir = tempfile::tempdir().unwrap();
        let (mut store, projection) = store_and_projection(&dir);
        // A lead whose remote and comp are both unknown at ingest time.
        let mut outcome = outcome("body");
        outcome.url = Some("https://example.com/job/14".into());
        outcome.extracted.title = Some("Staff Engineer".into());
        outcome.extracted.company = Some("Acme".into());
        record_ingest(&mut store, &projection, &Config::default(), &[], outcome).unwrap();

        let events = store.replay().unwrap();
        let projection = projections::rebuild(&events).unwrap();
        let record = projection.leads.values().next().unwrap();

        let captured = Arc::new(Mutex::new(Vec::new()));
        let layer = CapturingLayer {
            fields: captured.clone(),
        };
        let subscriber = tracing_subscriber::registry().with(layer);
        tracing::subscriber::with_default(subscriber, || {
            mark_lead(&mut store, &Config::default(), record, Mark::Defer, None).unwrap();
        });

        let fields = captured.lock().unwrap();
        let names: Vec<&str> = fields.iter().map(|(n, _)| n.as_str()).collect();
        assert!(
            names.contains(&"mark"),
            "mark span should carry mark: {fields:?}"
        );
        assert!(
            names.contains(&"remote_known"),
            "mark span should carry remote_known: {fields:?}"
        );
        assert!(
            names.contains(&"comp_known"),
            "mark span should carry comp_known: {fields:?}"
        );
        // The mark value is the defer mark; both statuses are unknown (false).
        assert!(
            fields
                .iter()
                .any(|(n, v)| n == "mark" && v.contains("defer")),
            "mark value: {fields:?}"
        );
        assert!(
            fields
                .iter()
                .any(|(n, v)| n == "remote_known" && v.contains("false")),
            "remote_known should be false: {fields:?}"
        );
    }
}
