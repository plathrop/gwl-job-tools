use miette::{IntoDiagnostic, Result};
use serde::Serialize;
use serde_with::skip_serializing_none;
use tracing::instrument;
use uuid::Uuid;

use super::open_workspace;
use crate::{cli::ListArgs, config::AppPaths, projections::LeadRecord, render};

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
    /// The derived lifecycle status (design doc 0002): the single
    /// application-stage dimension; mark/outcome above remain the facts.
    pub status: String,
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
        status: record.lifecycle_status(),
    }
}

#[instrument(skip_all)]
pub async fn execute_list(args: ListArgs, paths: &AppPaths, json: bool, color: bool) -> Result<()> {
    // `list` only reads the projection, so the store (and its single-writer
    // lock) can be released as soon as the projection is built.
    let (_, projection) = open_workspace(paths)?;

    // The default view is the active pipeline (design doc 0002): every lead
    // that is neither terminal nor durably ignored — pending, deferred,
    // applying, applied, screened, … (and gate-rejected, at the bottom,
    // `edit`-revivable) — ranked by score. `--all` adds the terminal and
    // ignored leads back in. The pending review queue itself remains what
    // `review` steps through; `list` no longer duplicates it. One shared
    // sort (PR #16 review).
    let records: Vec<&LeadRecord> = if args.all {
        projection.ranked_leads()
    } else {
        projection.active_leads()
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::test_support::*;

    #[test]
    fn queue_entry_serializes_the_json_row_shape() {
        // L7: queue_entry/QueueEntry had no direct test of the JSON row
        // shape.
        let mut record = lead_record(Some("https://example.com/j"));
        record.extracted.title = Some("Staff Engineer".into());
        record.extracted.company = Some("Acme".into());
        record.latest_mark = Some("defer".into());
        record.deferral_count = 2;

        let entry = queue_entry(3, &record);
        let json = serde_json::to_value(&entry).unwrap();

        assert_eq!(json["rank"], 3);
        assert_eq!(json["title"], "Staff Engineer");
        assert_eq!(json["company"], "Acme");
        assert_eq!(json["deferral_count"], 2);
        assert_eq!(json["mark"], "defer");
        assert_eq!(json["status"], "deferred");
        // composite and outcome are None → omitted (skip_serializing_none).
        assert!(json.get("composite").is_none());
        assert!(json.get("outcome").is_none());
    }
}
