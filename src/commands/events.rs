use miette::{IntoDiagnostic, Result};
use tracing::instrument;
use uuid::Uuid;

use super::select_lead;
use crate::{
    cli::EventsArgs,
    config::AppPaths,
    domain::{events::EventEnvelope, lead},
    event_store::{EventStore, JsonlEventStore},
    projections,
};

#[instrument(skip_all)]
pub async fn execute_events(args: EventsArgs, paths: &AppPaths) -> Result<()> {
    let store = JsonlEventStore::open(paths.event_log())?;
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
    use crate::{
        commands::{record_ingest, test_support::*},
        config::Config,
    };

    #[tokio::test]
    async fn events_lead_resolves_prefix_with_errors() {
        // Copilot: `--lead` is documented as an unambiguous UUID prefix;
        // unlike the other lead-addressed commands the filter silently
        // emits all matching streams and silently succeeds on no match.
        // The prefix must resolve once, with the zero/multiple-match errors.
        let dir = tempfile::tempdir().unwrap();
        let mut store = JsonlEventStore::open(dir.path().join("events.jsonl")).unwrap();
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

    #[test]
    fn filter_events_type_filter_selects_only_matching_type() {
        // L7: the `--type` filter path was untested (only lead-prefix
        // resolution had coverage).
        let lead = Uuid::new_v4();
        let now = jiff::Timestamp::now();
        let env = |stream: &str, event_type: &str| EventEnvelope {
            envelope_version: crate::domain::events::ENVELOPE_VERSION,
            id: Uuid::now_v7(),
            stream: stream.to_string(),
            seq: 1,
            event_type: event_type.to_string(),
            schema_version: 1,
            occurred_at: now,
            recorded_at: now,
            causation_id: None,
            correlation_id: Uuid::now_v7(),
            payload: serde_json::json!({}),
        };

        let events = vec![
            env(&format!("lead/{lead}"), "ingested"),
            env(&format!("lead/{lead}"), "scored"),
            env(&format!("lead/{lead}"), "reviewed"),
        ];

        // Type-only filter.
        let scored = filter_events(&events, None, Some("scored"));
        assert_eq!(scored.len(), 1);
        assert_eq!(scored[0].event_type, "scored");

        // Combined lead + type filter: a scored event on another lead is
        // excluded.
        let other = Uuid::new_v4();
        let mut mixed = events;
        mixed.push(env(&format!("lead/{other}"), "scored"));
        let filtered = filter_events(&mixed, Some(lead), Some("scored"));
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].stream, format!("lead/{lead}"));
    }
}
