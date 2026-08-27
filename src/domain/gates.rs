//! Hard filters (spec §2, design doc 0001 §3 `rejected` event). Binary
//! reject, not scored: a lead failing any gate is durably recorded via a
//! `rejected { gate, reason }` event.
//!
//! Gate philosophy (confirmed 2026-08-26): gates reject only on
//! high-confidence negatives. A false rejection is an invisible loss (the
//! lead never reaches the human); a false pass costs one glance at a review
//! card. Uncertainty passes to review and is surfaced there.

use serde::Serialize;

use crate::{
    config::Config,
    domain::{events::ExtractedFields, identity::slugify},
};

/// Gate identifiers, matching the `gate` enum on `rejected` payloads
/// (design doc 0001 §3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum Gate {
    #[serde(rename = "remote-only")]
    RemoteOnly,
    #[serde(rename = "compensation-floor")]
    CompensationFloor,
    #[serde(rename = "blacklist")]
    Blacklist,
    #[serde(rename = "ideological")]
    Ideological,
}

impl Gate {
    pub fn as_str(&self) -> &'static str {
        match self {
            Gate::RemoteOnly => "remote-only",
            Gate::CompensationFloor => "compensation-floor",
            Gate::Blacklist => "blacklist",
            Gate::Ideological => "ideological",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct GateFailure {
    pub gate: Gate,
    pub reason: String,
}

/// Evaluate all gates against an extracted posting. Returns every failure
/// (a lead may fail several gates at once; each becomes a `rejected`
/// event).
///
/// - **remote-only**: rejects only confident non-remote
///   (`remote == Some(false)`; `None` passes — see module docs).
/// - **compensation-floor**: rejects when the quoted range tops out below
///   the floor (uses `max` when present, else `min`). Unknown/missing comp
///   passes (settled decision); the comp dimension then drops out of the
///   composite with weight renormalization (Increment 3).
/// - **blacklist**: slug-substring match on company. Never match
///   blacklisted companies.
/// - **ideological**: the mechanism ships in v0 as a filter list over the
///   posting text; the content is empty until the LLM scorer (Remi) lands.
pub fn evaluate(config: &Config, extracted: &ExtractedFields, raw_text: &str) -> Vec<GateFailure> {
    let mut failures = Vec::new();

    if config.remote_only && extracted.remote == Some(false) {
        failures.push(GateFailure {
            gate: Gate::RemoteOnly,
            reason: format!(
                "confident non-remote signals (location: {})",
                extracted.location.as_deref().unwrap_or("none")
            ),
        });
    }

    if let (Some(floor), Some(comp)) = (config.compensation_floor, &extracted.comp) {
        // Compare against the top of the quoted range when present, else
        // the single/bottom figure. (Comp is normalized to USD/year at
        // extraction.)
        let quoted = comp.max.or(comp.min);
        if let Some(quoted) = quoted
            && quoted < floor
        {
            failures.push(GateFailure {
                gate: Gate::CompensationFloor,
                reason: format!("quoted max ${quoted} below floor ${floor}"),
            });
        }
    }

    if let Some(company) = &extracted.company {
        // Compare on fully alphanumeric-folded forms so "Salesforce",
        // "Sales Force", and "Salesforce, Inc." all match.
        let company_folded = slugify(company).replace('-', "");
        for blocked in &config.blacklist {
            let blocked_folded = slugify(blocked).replace('-', "");
            if !blocked_folded.is_empty() && company_folded.contains(&blocked_folded) {
                failures.push(GateFailure {
                    gate: Gate::Blacklist,
                    reason: format!("company '{company}' matches blacklist entry '{blocked}'"),
                });
            }
        }
    }

    for red_line in &config.ideological_red_lines {
        if !red_line.is_empty()
            && let Some(pos) = raw_text.to_lowercase().find(&red_line.to_lowercase())
        {
            let start = raw_text.floor_char_boundary(pos);
            let end = raw_text.ceil_char_boundary((pos + red_line.len() + 40).min(raw_text.len()));
            failures.push(GateFailure {
                gate: Gate::Ideological,
                reason: format!(
                    "posting matches red line '{red_line}': …{}…",
                    &raw_text[start..end]
                ),
            });
        }
    }

    failures
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::events::CompRange;

    fn config() -> Config {
        Config {
            compensation_floor: Some(180_000),
            remote_only: true,
            blacklist: vec!["salesforce".into()],
            ..Default::default()
        }
    }

    fn extracted() -> ExtractedFields {
        ExtractedFields {
            title: Some("Staff Engineer".into()),
            company: Some("Acme".into()),
            location: Some("Remote, US".into()),
            remote: Some(true),
            comp: Some(CompRange {
                min: Some(190_000),
                max: Some(240_000),
                currency: "USD".into(),
                period: "year".into(),
                raw: "$190,000 - $240,000".into(),
            }),
            ..Default::default()
        }
    }

    // ── remote-only ──────────────────────────────────────────────

    #[test]
    fn remote_confident_true_passes() {
        assert!(evaluate(&config(), &extracted(), "body").is_empty());
    }

    #[test]
    fn remote_confident_false_rejected() {
        let mut e = extracted();
        e.remote = Some(false);
        e.location = Some("San Francisco, CA".into());
        let failures = evaluate(&config(), &e, "body");
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].gate, Gate::RemoteOnly);
        assert!(failures[0].reason.contains("San Francisco, CA"));
    }

    #[test]
    fn remote_unknown_passes() {
        let mut e = extracted();
        e.remote = None;
        assert!(evaluate(&config(), &e, "body").is_empty());
    }

    #[test]
    fn remote_gate_off_when_not_configured() {
        let mut cfg = config();
        cfg.remote_only = false;
        let mut e = extracted();
        e.remote = Some(false);
        assert!(evaluate(&cfg, &e, "body").is_empty());
    }

    // ── compensation floor ───────────────────────────────────────

    #[test]
    fn comp_below_floor_rejected() {
        let mut e = extracted();
        e.comp = Some(CompRange {
            min: Some(120_000),
            max: Some(140_000),
            currency: "USD".into(),
            period: "year".into(),
            raw: "$120,000 - $140,000".into(),
        });
        let failures = evaluate(&config(), &e, "body");
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].gate, Gate::CompensationFloor);
        assert!(failures[0].reason.contains("$140000 below floor $180000"));
    }

    #[test]
    fn comp_unknown_passes_floor() {
        let mut e = extracted();
        e.comp = None;
        assert!(evaluate(&config(), &e, "body").is_empty());
    }

    #[test]
    fn comp_single_amount_below_floor_rejected() {
        let mut e = extracted();
        e.comp = Some(CompRange {
            min: Some(150_000),
            max: None,
            currency: "USD".into(),
            period: "year".into(),
            raw: "$150,000".into(),
        });
        let failures = evaluate(&config(), &e, "body");
        assert_eq!(failures[0].gate, Gate::CompensationFloor);
    }

    #[test]
    fn no_floor_configured_passes() {
        let mut cfg = config();
        cfg.compensation_floor = None;
        let mut e = extracted();
        e.comp = None;
        assert!(evaluate(&cfg, &e, "body").is_empty());
    }

    // ── blacklist ────────────────────────────────────────────────

    #[test]
    fn blacklisted_company_rejected() {
        let mut e = extracted();
        e.company = Some("Salesforce".into());
        let failures = evaluate(&config(), &e, "body");
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].gate, Gate::Blacklist);
    }

    #[test]
    fn blacklist_matches_variants() {
        for company in ["salesforce", "Salesforce, Inc.", "Sales force"] {
            let mut e = extracted();
            e.company = Some(company.into());
            let failures = evaluate(&config(), &e, "body");
            assert!(
                failures.iter().any(|f| f.gate == Gate::Blacklist),
                "{company} should be rejected"
            );
        }
    }

    #[test]
    fn unknown_company_passes_blacklist() {
        let mut e = extracted();
        e.company = None;
        assert!(evaluate(&config(), &e, "body").is_empty());
    }

    // ── ideological red lines (mechanism, empty content) ─────────

    #[test]
    fn empty_red_lines_reject_nothing() {
        assert!(config().ideological_red_lines.is_empty());
        assert!(evaluate(&config(), &extracted(), "any body at all").is_empty());
    }

    #[test]
    fn configured_red_line_rejects_with_context() {
        let mut cfg = config();
        cfg.ideological_red_lines = vec!["crypto".into()];
        let failures = evaluate(&config(), &extracted(), "We are not a crypto company.");
        assert!(failures.is_empty(), "config without red lines passes");
        let failures = evaluate(&cfg, &extracted(), "We are not a crypto company.");
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].gate, Gate::Ideological);
        assert!(failures[0].reason.contains("crypto"));
    }

    // ── multiple gates at once ───────────────────────────────────

    #[test]
    fn multiple_failures_all_reported() {
        let mut e = extracted();
        e.remote = Some(false);
        e.company = Some("Salesforce".into());
        e.comp = None;
        let failures = evaluate(&config(), &e, "body");
        assert_eq!(failures.len(), 2);
    }

    // ── serde ────────────────────────────────────────────────────

    #[test]
    fn gate_serializes_as_design_doc_enum() {
        assert_eq!(
            serde_json::to_value(Gate::RemoteOnly).unwrap(),
            "remote-only"
        );
        assert_eq!(
            serde_json::to_value(Gate::CompensationFloor).unwrap(),
            "compensation-floor"
        );
        assert_eq!(
            serde_json::to_value(Gate::Ideological).unwrap(),
            "ideological"
        );
    }
}
