use std::iter::once;

use miette::{IntoDiagnostic, Result, bail, miette};
use serde::Serialize;
use serde_with::skip_serializing_none;
use tracing::{debug, info, instrument};
use url::Url;
use uuid::Uuid;

use super::{open_workspace, replay_lead, select_lead};
use crate::{
    cli::{ClearField, EditArgs, RemoteState},
    config::{AppPaths, Config},
    domain::{
        events::{CompRange, ExtractedFields},
        gates::{self, GateFailure},
        identity,
        lead::{self, LeadState},
        scoring::{self, ScoreResult},
    },
    event_store::EventStore,
    projections::{self, LeadRecord, Projection},
    render, resume,
};

#[instrument(skip_all)]
pub async fn execute_edit(
    args: EditArgs,
    config: &Config,
    paths: &AppPaths,
    json: bool,
    color: bool,
) -> Result<()> {
    // Load the resume before anything else (like ingest): scoring uses the
    // resume skills, and a configured-but-broken resume must fail loudly
    // (decision 0004) — before any events are appended.
    let resume = resume::load(config.resume_path.as_deref())?;
    let resume_skills = resume
        .as_ref()
        .map(resume::Resume::keywords)
        .unwrap_or_default();

    let (mut store, projection) = open_workspace(paths)?;

    let record = select_lead(&projection, &args.lead)?;
    let spec = build_edit_spec(record, &args)?;
    let summary = record_edit(
        &mut store,
        &projection,
        config,
        &resume_skills,
        record,
        spec,
    )?;

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&summary).into_diagnostic()?
        );
    } else {
        // Re-render the card so the corrected fields and the new score are
        // visible immediately (same UX as ingest).
        let events = store.replay()?;
        let projection = projections::rebuild(&events)?;
        let record = projection
            .leads
            .get(&summary.lead_id)
            .ok_or_else(|| miette!("lead {} not found after edit", summary.lead_id))?;
        render::render_card(record, color)?;
    }
    Ok(())
}

/// Thousands-separated digits (`220000` → `220,000`) for the synthesized
/// comp display string — Rust format strings have no numeric grouping, and
/// the card should read like the parsed form does.
fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let bytes = digits.as_bytes();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

/// The merged result of an edit command's flags: the lead's corrected
/// snapshot. Built separately from appending so the flag-merge logic is
/// testable without a store.
#[derive(Clone, Debug)]
pub struct EditSpec {
    pub extracted: ExtractedFields,
    /// The final canonical posting URL (current URL when `--url` is absent).
    pub url: Option<String>,
    pub source: String,
    pub note: Option<String>,
}

/// Apply the edit flags to the lead's current snapshot (decision record
/// 0009). Unset flags keep the current value; `--clear` resets a field to
/// absent (explicit, not empty-string sentinels); `--remote` is tri-state
/// (`true`/`false`/`unknown`). `--comp` parses through the same extractor
/// the ingest path uses; `--comp-min`/`--comp-max` set exact USD/year bounds.
/// A field that is both set and cleared is a contradiction and bails —
/// silently discarding the set value is the footgun this prevents
/// (PR #15 review).
pub fn build_edit_spec(record: &LeadRecord, args: &EditArgs) -> Result<EditSpec> {
    // Contradiction check first, so `--comp "garbage" --clear comp` reports
    // the contradiction rather than a parse error for a value that is about
    // to be discarded anyway.
    let clears = |field: ClearField| args.clear.contains(&field);
    let contradiction = |field: &str| {
        bail!(
            "{field} is both set and cleared; use one or the other (a clear replaces the value, it does not combine with it)"
        );
    };
    if args.title.is_some() && clears(ClearField::Title) {
        contradiction("--title vs --clear title")?;
    }
    if args.company.is_some() && clears(ClearField::Company) {
        contradiction("--company vs --clear company")?;
    }
    if args.req_id.is_some() && clears(ClearField::ReqId) {
        contradiction("--req-id vs --clear req_id")?;
    }
    if args.location.is_some() && clears(ClearField::Location) {
        contradiction("--location vs --clear location")?;
    }
    if args.remote.is_some_and(|r| r != RemoteState::Unknown) && clears(ClearField::Remote) {
        // `--remote unknown` agrees with `--clear remote` (both clear the
        // signal); a confident value contradicts it.
        contradiction("--remote vs --clear remote")?;
    }
    let comp_set = args.comp.is_some() || args.comp_min.is_some() || args.comp_max.is_some();
    if comp_set && clears(ClearField::Comp) {
        contradiction("--comp vs --clear comp")?;
    }

    let mut extracted = record.extracted.clone();

    if let Some(title) = &args.title {
        extracted.title = Some(title.clone());
    }
    if let Some(company) = &args.company {
        extracted.company = Some(company.clone());
    }
    if let Some(req_id) = &args.req_id {
        extracted.req_id = Some(req_id.clone());
    }
    if let Some(location) = &args.location {
        extracted.location = Some(location.clone());
    }
    if let Some(remote) = args.remote {
        remote.apply(&mut extracted.remote);
    }
    if let Some(comp) = &args.comp {
        let parsed = crate::ingest::extract::extract_comp(comp).ok_or_else(|| {
            miette!(
                "could not parse --comp '{comp}' (expected e.g. \"$220,000 - $290,000\" or \"$180,000/yr\")"
            )
        })?;
        extracted.comp = Some(parsed);
    } else if args.comp_min.is_some() || args.comp_max.is_some() {
        if let (Some(min), Some(max)) = (args.comp_min, args.comp_max)
            && min > max
        {
            bail!("--comp-min {min} exceeds --comp-max {max}");
        }
        let raw = match (args.comp_min, args.comp_max) {
            (Some(min), Some(max)) => format!("${} - ${}", thousands(min), thousands(max)),
            (Some(min), None) => format!("${}", thousands(min)),
            (None, Some(max)) => format!("${}", thousands(max)),
            (None, None) => {
                unreachable!("guarded by the is_some check above")
            }
        };
        extracted.comp = Some(CompRange {
            min: args.comp_min,
            max: args.comp_max,
            currency: "USD".into(),
            period: "year".into(),
            raw,
        });
    }
    // `--clear` applies only to fields with no set flag (checked above), so
    // it can never silently discard a value the user also provided.
    for field in &args.clear {
        match field {
            ClearField::Title => extracted.title = None,
            ClearField::Company => extracted.company = None,
            ClearField::ReqId => extracted.req_id = None,
            ClearField::Location => extracted.location = None,
            ClearField::Remote => extracted.remote = None,
            ClearField::Comp => extracted.comp = None,
        }
    }

    let url = match &args.url {
        Some(url) => Some(identity::canonicalize_url(url)),
        None => record.url.clone(),
    };
    let source = args
        .source
        .map(|s| s.as_str().to_string())
        .or_else(|| record.source.clone())
        .unwrap_or_else(crate::domain::events::default_lead_source);

    Ok(EditSpec {
        extracted,
        url,
        source,
        note: args.note.clone(),
    })
}

/// The testable core of `gwl-jobs edit` (decision record 0009): recompute
/// identifiers from the corrected fields (additive, collision-checked),
/// re-run gates and scoring on the corrected content, and append the
/// `edited` + `rejected`/`scored` batch.
pub fn record_edit(
    store: &mut impl EventStore,
    projection: &Projection,
    config: &Config,
    resume_skills: &[String],
    record: &LeadRecord,
    spec: EditSpec,
) -> Result<EditSummary> {
    let lead_id = record.lead_id;
    // The dedupe key is the lead's durable identity: it never changes on an
    // edit (decision record 0009). The recomputed key is checked for
    // collisions below but not stored.
    let dedupe_key = record
        .dedupe_key
        .clone()
        .ok_or_else(|| miette!("lead {lead_id} has no dedupe key (corrupt record)"))?;

    let state = replay_lead(store, lead_id)?;
    let raw_text = state.raw_text.clone();

    // Identifiers are recomputed from the corrected fields so future
    // re-ingests of the corrected posting match this lead. Indexing is
    // additive (old forms stay), so only a collision with a *different*
    // lead is a problem.
    let url_parsed = spec.url.as_deref().and_then(|u| Url::parse(u).ok());
    let recomputed = identity::compute_identity(
        &spec.extracted,
        url_parsed.as_ref(),
        raw_text.as_deref().unwrap_or(""),
    );
    for form in recomputed
        .identifiers
        .req
        .iter()
        .chain(recomputed.identifiers.url.iter())
        .chain(recomputed.identifiers.tc.iter())
        .chain(once(&recomputed.dedupe_key))
    {
        if let Some(owner) = projection.identifier_owner(form)
            && owner != lead_id
        {
            bail!("edit would collide with lead {owner} on identity {form}");
        }
    }

    // An edit re-evaluates the lead like a re-ingest does (decision record
    // 0009): gates run on the corrected content, and the batch carries
    // `rejected` XOR `scored`. Durably ignored leads are NOT suppressed —
    // the edit is explicit user action — but the mark stays latest-wins, so
    // an ignored lead still won't re-enter the queue.
    let gate_failures = gates::evaluate(config, &spec.extracted, raw_text.as_deref().unwrap_or(""));
    let score = if gate_failures.is_empty() {
        Some(scoring::score(
            config,
            &spec.extracted,
            raw_text.as_deref().unwrap_or(""),
            resume_skills,
        ))
    } else {
        None
    };
    if let Some(score) = &score {
        debug!(
            composite = score.composite,
            breakdown = %score.breakdown,
            "lead re-scored after edit"
        );
    }

    let (changed, pending) = lead::decide_edit(
        &state,
        &dedupe_key,
        &recomputed.identifiers,
        spec.note.clone(),
        &spec.source,
        spec.url.clone(),
        raw_text,
        spec.extracted.clone(),
        gate_failures.clone(),
        score.clone(),
    )?;

    let stream = LeadState::stream_id(lead_id);
    store.append(&stream, state.seq, &pending, Uuid::now_v7())?;
    info!(%lead_id, ?changed, "lead edited");

    Ok(EditSummary {
        lead_id,
        changed,
        rejected: if gate_failures.is_empty() {
            None
        } else {
            Some(gate_failures)
        },
        score,
        dedupe_key,
        source: spec.source,
        url: spec.url,
        extracted: spec.extracted,
        note: spec.note,
    })
}

/// The `gwl-jobs edit --json` output.
#[skip_serializing_none]
#[derive(Clone, Debug, Serialize)]
pub struct EditSummary {
    pub lead_id: Uuid,
    pub changed: Vec<String>,
    pub rejected: Option<Vec<GateFailure>>,
    pub score: Option<ScoreResult>,
    pub dedupe_key: String,
    pub source: String,
    pub url: Option<String>,
    pub extracted: ExtractedFields,
    /// The provenance note recorded on the `edited` event (echoed so the
    /// `--json` summary matches the event it produced).
    pub note: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        cli::Mark,
        commands::{load_raw_text, mark_lead, record_ingest, test_support::*},
        event_store::JsonlEventStore,
    };

    fn edit_args() -> EditArgs {
        EditArgs::default()
    }

    #[test]
    fn build_edit_spec_merges_flags_over_current_values() {
        let record = lead_record(Some("https://example.com/j"));
        let args = edit_args();
        let spec = build_edit_spec(&record, &args).unwrap();
        // Unset flags keep the current values; a record with no source
        // defaults to `unknown`.
        assert_eq!(spec.extracted, ExtractedFields::default());
        assert_eq!(spec.url.as_deref(), Some("https://example.com/j"));
        assert_eq!(spec.source, "unknown");
    }

    #[test]
    fn build_edit_spec_applies_flags_and_clears() {
        let mut record = lead_record(Some("https://example.com/j"));
        record.extracted.title = Some("Staff DevOps Engineer".into());
        record.extracted.remote = Some(false);
        record.source = Some("search".into());

        let mut args = edit_args();
        args.title = Some("Staff DevOps Engineer, API Platform".into());
        args.remote = Some(RemoteState::Unknown);
        args.source = Some(crate::cli::LeadSource::Recruiter);
        args.clear = vec![crate::cli::ClearField::Remote];

        let spec = build_edit_spec(&record, &args).unwrap();
        assert_eq!(
            spec.extracted.title.as_deref(),
            Some("Staff DevOps Engineer, API Platform")
        );
        // `--remote unknown` and `--clear remote` agree: the signal is gone
        // (a confident `--remote` value plus a clear would bail as a
        // contradiction — see build_edit_spec_contradictory_flags_bail).
        assert_eq!(spec.extracted.remote, None);
        assert_eq!(spec.source, "recruiter");
    }

    #[test]
    fn build_edit_spec_parses_comp_like_extraction() {
        let record = lead_record(None);
        let mut args = edit_args();
        args.comp = Some("$220,000 - $290,000".into());
        let comp = build_edit_spec(&record, &args)
            .unwrap()
            .extracted
            .comp
            .unwrap();
        assert_eq!(comp.min, Some(220_000));
        assert_eq!(comp.max, Some(290_000));
        assert_eq!(comp.period, "year");

        // Hourly/single amounts parse too: "$180,000/yr" → min only.
        let mut args = edit_args();
        args.comp = Some("$180,000/yr".into());
        let comp = build_edit_spec(&record, &args)
            .unwrap()
            .extracted
            .comp
            .unwrap();
        assert_eq!(comp.min, Some(180_000));
        assert_eq!(comp.max, None);

        // Unparseable comp is a loud failure, not a silent drop.
        let mut args = edit_args();
        args.comp = Some("competitive".into());
        assert!(build_edit_spec(&record, &args).is_err());
    }

    #[test]
    fn build_edit_spec_contradictory_flags_bail() {
        // PR #15: a field both set and cleared must not silently
        // discard the set value — it is a contradiction, and a loud one.
        let record = lead_record(None);

        let mut args = edit_args();
        args.title = Some("Engineer".into());
        args.clear = vec![crate::cli::ClearField::Title];
        assert!(build_edit_spec(&record, &args).is_err());

        // The comp contradiction fires BEFORE parsing: an unparseable value
        // that is about to be cleared reports the contradiction, not a
        // parse error for a discarded string.
        let mut args = edit_args();
        args.comp = Some("garbage".into());
        args.clear = vec![crate::cli::ClearField::Comp];
        let err = format!("{}", build_edit_spec(&record, &args).unwrap_err());
        assert!(err.contains("both set and cleared"), "got: {err}");

        let mut args = edit_args();
        args.comp_min = Some(220_000);
        args.clear = vec![crate::cli::ClearField::Comp];
        assert!(build_edit_spec(&record, &args).is_err());

        // A confident remote value contradicts a clear; `--remote unknown`
        // agrees with it (both clear the signal).
        let mut args = edit_args();
        args.remote = Some(RemoteState::True);
        args.clear = vec![crate::cli::ClearField::Remote];
        assert!(build_edit_spec(&record, &args).is_err());
        let mut args = edit_args();
        args.remote = Some(RemoteState::Unknown);
        args.clear = vec![crate::cli::ClearField::Remote];
        assert!(build_edit_spec(&record, &args).is_ok());
    }

    #[test]
    fn build_edit_spec_exact_comp_bounds() {
        let record = lead_record(None);
        let mut args = edit_args();
        args.comp_min = Some(220_000);
        args.comp_max = Some(290_000);
        let comp = build_edit_spec(&record, &args)
            .unwrap()
            .extracted
            .comp
            .unwrap();
        assert_eq!(comp.min, Some(220_000));
        assert_eq!(comp.max, Some(290_000));
        assert_eq!(comp.currency, "USD");
        assert_eq!(comp.period, "year");
        // The synthesized display string matches the parsed form's style
        // (PR #15 review nit: no bare "220000 - 290000" on the card).
        assert_eq!(comp.raw, "$220,000 - $290,000");

        let mut args = edit_args();
        args.comp_min = Some(220_000);
        let comp = build_edit_spec(&record, &args)
            .unwrap()
            .extracted
            .comp
            .unwrap();
        assert_eq!(comp.raw, "$220,000");

        let mut args = edit_args();
        args.comp_min = Some(300_000);
        args.comp_max = Some(200_000);
        assert!(build_edit_spec(&record, &args).is_err());
    }

    #[test]
    fn build_edit_spec_canonicalizes_url() {
        let record = lead_record(Some("https://example.com/old"));
        let mut args = edit_args();
        args.url = Some(Url::parse("https://example.com/new?utm_source=li").unwrap());
        let spec = build_edit_spec(&record, &args).unwrap();
        assert_eq!(spec.url.as_deref(), Some("https://example.com/new"));
    }

    #[test]
    fn edit_rescores_and_reenters_queue() {
        // The motivating case (GWLJ-3gd5w0): a Wellfound-style ingest where
        // comp and remote could not be extracted. The user corrects both;
        // the lead re-evaluates with the compensation and remote dimensions
        // actually populated.
        let config = Config {
            compensation_floor: Some(180_000),
            compensation_ceiling: Some(400_000),
            ..Config::default()
        };
        let dir = tempfile::tempdir().unwrap();
        let (mut store, record) = ingested_lead(
            &dir,
            &config,
            ExtractedFields {
                title: Some("Staff DevOps Engineer - API Platform".into()),
                company: Some("TrustIn".into()),
                ..Default::default()
            },
            "a posting body with no comp and no remote signal",
        );

        let mut args = edit_args();
        args.comp = Some("$220,000 - $290,000".into());
        args.remote = Some(RemoteState::True);
        args.note = Some("recruiter email quoted the band".into());
        let spec = build_edit_spec(&record, &args).unwrap();
        let projection = projections::rebuild(&store.replay().unwrap()).unwrap();
        let summary = record_edit(&mut store, &projection, &config, &[], &record, spec).unwrap();

        assert_eq!(summary.lead_id, record.lead_id);
        // The --json summary echoes the note it recorded on the event
        // (PR #15 review nit).
        assert_eq!(
            summary.note.as_deref(),
            Some("recruiter email quoted the band")
        );
        // Field order follows ExtractedFields::diff: remote before comp; the
        // adapter flip drop-in → user (decision record 0009) closes the list.
        assert_eq!(summary.changed, vec!["remote", "comp", "adapter"]);
        assert!(summary.rejected.is_none());
        let score = summary.score.as_ref().unwrap();
        // The compensation dimension is present now (no longer renormalized
        // away): max $290k against floor $180k / ceiling $400k → 50.
        // Confident remote scores 100; level "Staff" scores 100. Equal
        // default weights, skills dropped (no resume) → (100+50+100)/3.
        let compensation = score
            .dimensions
            .iter()
            .find(|d| d.name == "compensation")
            .unwrap();
        assert_eq!(compensation.score, 50);
        assert_eq!(score.composite, 83);

        // The batch carries edited + scored (one correlation, causation
        // chained), and the lead is back in the pending queue.
        let events = store.replay().unwrap();
        let scored = events.last().unwrap();
        let edited = &events[events.len() - 2];
        assert_eq!(edited.event_type, "edited");
        assert_eq!(scored.event_type, "scored");
        assert_eq!(edited.correlation_id, scored.correlation_id);
        assert_eq!(scored.causation_id, Some(edited.id));
        assert_eq!(scored.payload["revision"], 2);

        let projection = projections::rebuild(&events).unwrap();
        let pending = projection.pending_queue();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].lead_id, record.lead_id);
    }

    #[test]
    fn edit_fixing_gate_failure_clears_rejection() {
        let config = Config {
            remote_only: true,
            ..Default::default()
        };
        let dir = tempfile::tempdir().unwrap();
        let (mut store, record) = ingested_lead(
            &dir,
            &config,
            ExtractedFields {
                title: Some("Engineer".into()),
                company: Some("Acme".into()),
                remote: Some(false),
                ..Default::default()
            },
            "on-site role in San Francisco",
        );
        let projection = projections::rebuild(&store.replay().unwrap()).unwrap();
        assert!(
            projection
                .leads
                .get(&record.lead_id)
                .unwrap()
                .latest_rejection
                .is_some()
        );

        // The user knows better: the role is actually remote.
        let mut args = edit_args();
        args.remote = Some(RemoteState::True);
        let spec = build_edit_spec(&record, &args).unwrap();
        let projection = projections::rebuild(&store.replay().unwrap()).unwrap();
        let summary = record_edit(&mut store, &projection, &config, &[], &record, spec).unwrap();
        assert_eq!(summary.changed, vec!["remote", "adapter"]);
        // Fixing a gate failure re-scores: the summary carries the new score,
        // not just the projection.
        assert!(summary.score.is_some());

        let projection = projections::rebuild(&store.replay().unwrap()).unwrap();
        let record = projection.leads.get(&record.lead_id).unwrap();
        assert!(record.latest_rejection.is_none());
        assert_eq!(projection.pending_queue().len(), 1);
    }

    #[test]
    fn edit_keeps_dedupe_key_immutable_and_indexes_new_identity() {
        // Decision record 0009: the dedupe key never changes on an edit, but
        // identifiers recomputed from the corrected fields are indexed
        // additively — a future drop of the corrected posting matches.
        let dir = tempfile::tempdir().unwrap();
        let (mut store, record) = ingested_lead(
            &dir,
            &Config::default(),
            ExtractedFields {
                title: Some("Staff DevOps Engineer".into()),
                company: Some("TrustIn".into()),
                ..Default::default()
            },
            "body",
        );
        let original_dedupe = record.dedupe_key.clone().unwrap();

        let mut args = edit_args();
        args.title = Some("Staff DevOps Engineer, API Platform".into());
        let spec = build_edit_spec(&record, &args).unwrap();
        let projection = projections::rebuild(&store.replay().unwrap()).unwrap();
        let summary = record_edit(
            &mut store,
            &projection,
            &Config::default(),
            &[],
            &record,
            spec,
        )
        .unwrap();
        assert_eq!(summary.dedupe_key, original_dedupe);

        let projection = projections::rebuild(&store.replay().unwrap()).unwrap();
        let updated = projection.leads.get(&record.lead_id).unwrap();
        assert_eq!(
            updated.dedupe_key.as_deref(),
            Some(original_dedupe.as_str())
        );

        // The corrected tc form matches this lead (additive indexing).
        let corrected = identity::compute_identity(
            &ExtractedFields {
                title: Some("Staff DevOps Engineer, API Platform".into()),
                company: Some("TrustIn".into()),
                ..Default::default()
            },
            None,
            "",
        );
        assert_eq!(projection.lookup(&corrected), Some(record.lead_id));
    }

    #[test]
    fn edit_identity_collision_bails() {
        // Correcting lead A into lead B's identity is a merge, not an edit —
        // refuse rather than silently re-pointing an index.
        let dir = tempfile::tempdir().unwrap();
        let config = Config::default();
        let (store, record_a) = ingested_lead(
            &dir,
            &config,
            ExtractedFields {
                title: Some("Engineer".into()),
                company: Some("Alpha".into()),
                ..Default::default()
            },
            "body a",
        );
        drop(store); // release the single-writer lock
        let (store_b, _record_b) = ingested_lead(
            &dir,
            &config,
            ExtractedFields {
                title: Some("Designer".into()),
                company: Some("Beta".into()),
                ..Default::default()
            },
            "body b",
        );
        drop(store_b);
        let mut store = JsonlEventStore::open(dir.path().join("events.jsonl")).unwrap();

        let mut args = edit_args();
        args.title = Some("Designer".into());
        args.company = Some("Beta".into());
        let spec = build_edit_spec(&record_a, &args).unwrap();
        let projection = projections::rebuild(&store.replay().unwrap()).unwrap();
        let result = record_edit(&mut store, &projection, &config, &[], &record_a, spec);
        assert!(result.is_err());
    }

    #[test]
    fn edit_with_no_effective_change_bails() {
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
        let spec = build_edit_spec(&record, &edit_args()).unwrap();
        let projection = projections::rebuild(&store.replay().unwrap()).unwrap();
        assert!(
            record_edit(
                &mut store,
                &projection,
                &Config::default(),
                &[],
                &record,
                spec
            )
            .is_err()
        );
        // Nothing was appended.
        assert_eq!(store.replay().unwrap().len(), 2);
    }

    #[test]
    fn edit_of_ignored_lead_records_but_stays_out_of_queue() {
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
        let projection = projections::rebuild(&store.replay().unwrap()).unwrap();
        let record = projection.leads.get(&record.lead_id).unwrap().clone();
        mark_lead(&mut store, &Config::default(), &record, Mark::Ignore, None).unwrap();

        // Edits bypass durable-ignore suppression (explicit user action) —
        // but the mark stays latest-wins, so the lead stays out of the
        // queue.
        let mut args = edit_args();
        args.location = Some("Remote, US".into());
        let spec = build_edit_spec(&record, &args).unwrap();
        let projection = projections::rebuild(&store.replay().unwrap()).unwrap();
        record_edit(
            &mut store,
            &projection,
            &Config::default(),
            &[],
            &record,
            spec,
        )
        .unwrap();

        let events = store.replay().unwrap();
        // ingested, scored, reviewed(ignore), edited, scored — the edited
        // evaluation passes gates, so its scored event follows in the same
        // batch.
        assert_eq!(events[3].event_type, "edited");
        assert_eq!(events.last().unwrap().event_type, "scored");
        let projection = projections::rebuild(&events).unwrap();
        assert!(projection.pending_queue().is_empty());
        assert_eq!(
            projection
                .leads
                .get(&record.lead_id)
                .unwrap()
                .latest_mark
                .as_deref(),
            Some("ignore")
        );
    }

    #[test]
    fn edit_source_only_change_reports_source() {
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
        let mut args = edit_args();
        args.source = Some(crate::cli::LeadSource::Recruiter);
        let spec = build_edit_spec(&record, &args).unwrap();
        let projection = projections::rebuild(&store.replay().unwrap()).unwrap();
        let summary = record_edit(
            &mut store,
            &projection,
            &Config::default(),
            &[],
            &record,
            spec,
        )
        .unwrap();
        // The edit also flips the adapter drop-in → user (decision record
        // 0009), which the changed list names alongside the source.
        assert_eq!(summary.changed, vec!["source", "adapter"]);
        assert_eq!(summary.source, "recruiter");
    }

    #[test]
    fn second_edit_does_not_rereport_adapter() {
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

        // First edit flips the adapter drop-in → user (decision record 0009)
        // and names it in the changed list.
        let mut args = edit_args();
        args.title = Some("Senior Engineer".into());
        let spec = build_edit_spec(&record, &args).unwrap();
        let projection = projections::rebuild(&store.replay().unwrap()).unwrap();
        let summary = record_edit(
            &mut store,
            &projection,
            &Config::default(),
            &[],
            &record,
            spec,
        )
        .unwrap();
        assert_eq!(summary.changed, vec!["title", "adapter"]);

        // Once the adapter has settled to "user", a second edit reports only
        // the field that actually changed — no adapter noise.
        let record = projection.leads.get(&record.lead_id).unwrap().clone();
        let mut args = edit_args();
        args.title = Some("Staff Engineer".into());
        let spec = build_edit_spec(&record, &args).unwrap();
        let projection = projections::rebuild(&store.replay().unwrap()).unwrap();
        let summary = record_edit(
            &mut store,
            &projection,
            &Config::default(),
            &[],
            &record,
            spec,
        )
        .unwrap();
        assert_eq!(summary.changed, vec!["title"]);
    }

    #[test]
    fn edit_carries_raw_text_forward() {
        let dir = tempfile::tempdir().unwrap();
        let (mut store, record) = ingested_lead(
            &dir,
            &Config::default(),
            ExtractedFields {
                title: Some("Engineer".into()),
                company: Some("Acme".into()),
                ..Default::default()
            },
            "the full JD text",
        );
        let mut args = edit_args();
        args.location = Some("Remote, US".into());
        let spec = build_edit_spec(&record, &args).unwrap();
        let projection = projections::rebuild(&store.replay().unwrap()).unwrap();
        record_edit(
            &mut store,
            &projection,
            &Config::default(),
            &[],
            &record,
            spec,
        )
        .unwrap();
        assert_eq!(
            load_raw_text(&store, record.lead_id).unwrap().as_deref(),
            Some("the full JD text")
        );
    }

    #[test]
    fn reingest_after_edit_clobbers_user_correction() {
        // Known limitation (decision record 0009, accepted): a re-ingest
        // snapshot replaces the whole snapshot, including fields the user
        // corrected. This test pins the behavior so a future fix (merge
        // protection over user-edited fields) has a baseline.
        let dir = tempfile::tempdir().unwrap();
        let config = Config::default();
        let (mut store, record) = ingested_lead(
            &dir,
            &config,
            ExtractedFields {
                title: Some("Staff DevOps Engineer".into()),
                ..Default::default()
            },
            "body v1",
        );

        let mut args = edit_args();
        args.company = Some("TrustIn".into());
        let spec = build_edit_spec(&record, &args).unwrap();
        let projection = projections::rebuild(&store.replay().unwrap()).unwrap();
        record_edit(&mut store, &projection, &config, &[], &record, spec).unwrap();
        let projection = projections::rebuild(&store.replay().unwrap()).unwrap();
        let record = projection.leads.get(&record.lead_id).unwrap().clone();
        assert_eq!(record.extracted.company.as_deref(), Some("TrustIn"));

        // A repost arrives via the original (company-less) extraction.
        let mut o = outcome("body v2");
        o.url = record.url.clone();
        o.extracted.title = Some("Staff DevOps Engineer".into());
        record_ingest(&mut store, &projection, &config, &[], o).unwrap();

        let projection = projections::rebuild(&store.replay().unwrap()).unwrap();
        let record = projection.leads.get(&record.lead_id).unwrap();
        assert_eq!(record.extracted.company, None);
    }
}
