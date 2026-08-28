//! Scoring (spec §3, design doc 0001 §3 `scored` event). Fully deterministic:
//! per-dimension 0–100 scores, each with a `confidence` field (default 1.0;
//! only meaningful once LLM scorers arrive), combined into a weighted-sum
//! composite with a human-readable breakdown.
//!
//! Dimensions: `level`, `skills`, `compensation`, `remote`. A dimension that
//! cannot be scored (unknown comp, no resume) drops out of the composite with
//! weight renormalization, and the breakdown notes it.

use std::collections::HashMap;

use crate::{
    config::Config,
    domain::events::{CompRange, DimensionScore, ExtractedFields},
};

/// The result of scoring a posting that passed all gates. `revision` is not
/// here — the aggregate fills it in (it counts gate/score evaluations).
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct ScoreResult {
    pub composite: u64,
    pub dimensions: Vec<DimensionScore>,
    pub breakdown: String,
}

/// Score a posting. `resume_skills` is the flattened resume keyword list
/// (empty when no resume is configured or it has no skills — the skills
/// dimension then drops out).
pub fn score(
    config: &Config,
    extracted: &ExtractedFields,
    raw_text: &str,
    resume_skills: &[String],
) -> ScoreResult {
    let mut dims: Vec<(&'static str, u64)> = Vec::new();
    let mut dropped: Vec<&'static str> = Vec::new();

    dims.push(("level", level_score(extracted, raw_text)));

    if resume_skills.is_empty() {
        dropped.push("skills");
    } else {
        dims.push((
            "skills",
            skills_score(raw_text, resume_skills, &config.aliases),
        ));
    }

    match compensation_score(config, extracted) {
        Some(s) => dims.push(("compensation", s)),
        None => dropped.push("compensation"),
    }

    dims.push(("remote", remote_score(extracted)));

    let dimensions: Vec<DimensionScore> = dims
        .iter()
        .map(|(name, score)| DimensionScore {
            name: (*name).to_string(),
            score: *score,
            weight: weight_of(config, name),
            confidence: 1.0,
        })
        .collect();

    let (composite, breakdown) = composite_and_breakdown(&dimensions, &dropped);

    ScoreResult {
        composite,
        dimensions,
        breakdown,
    }
}

fn weight_of(config: &Config, name: &str) -> f64 {
    match name {
        "level" => config.scoring_weights.level,
        "skills" => config.scoring_weights.skills,
        "compensation" => config.scoring_weights.compensation,
        "remote" => config.scoring_weights.remote,
        _ => 1.0,
    }
}

/// Weighted sum with renormalization over the present dimensions, plus the
/// human-readable breakdown (renormalized weights, dropped-dimension notes).
fn composite_and_breakdown(
    dimensions: &[DimensionScore],
    dropped: &[&'static str],
) -> (u64, String) {
    let total_weight: f64 = dimensions.iter().map(|d| d.weight).sum();
    let weighted_sum: f64 = dimensions.iter().map(|d| d.weight * d.score as f64).sum();
    let composite = if total_weight > 0.0 {
        (weighted_sum / total_weight).round() as u64
    } else {
        0
    };

    let parts: Vec<String> = dimensions
        .iter()
        .map(|d| {
            format!(
                "{}·{}({})",
                fmt_weight(d.weight / total_weight),
                d.name,
                d.score
            )
        })
        .collect();
    let mut breakdown = format!("{composite} = {}", parts.join(" + "));
    if !dropped.is_empty() {
        let notes: Vec<String> = dropped
            .iter()
            .map(|d| match *d {
                "compensation" => "compensation: unknown, weight renormalized".to_string(),
                "skills" => "skills: no resume, weight renormalized".to_string(),
                other => format!("{other}: unavailable, weight renormalized"),
            })
            .collect();
        breakdown.push_str(&format!(" [{}]", notes.join("; ")));
    }
    (composite, breakdown)
}

/// Format a weight for the breakdown: at most 3 decimals, trailing zeros
/// trimmed (`0.3`, `0.25`, `0.5`, `1`).
fn fmt_weight(w: f64) -> String {
    let s = format!("{w:.3}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s.is_empty() {
        "0".to_string()
    } else {
        s.to_string()
    }
}

// ── level ────────────────────────────────────────────────────────

/// Title is the primary signal (it directly encodes the target level);
/// quoted years-of-experience is the fallback for generic titles. No signal
/// → 50 (unknown).
fn level_score(extracted: &ExtractedFields, raw_text: &str) -> u64 {
    let title = extracted.title.as_deref().unwrap_or("").to_lowercase();
    if ["principal", "staff", "architect"]
        .iter()
        .any(|k| title.contains(k))
    {
        return 100;
    }
    if ["senior", "lead"].iter().any(|k| title.contains(k)) {
        return 70;
    }
    if ["junior", "associate", "entry", "intern"]
        .iter()
        .any(|k| title.contains(k))
    {
        return 10;
    }
    // No title signal: fall back to quoted years (15+ = 100), else unknown.
    if let Some(years) = extract_years(raw_text) {
        return years.min(15) * 100 / 15;
    }
    50
}

/// First number in a "X years" / "X+ years" / "X-Y years" context (the lower
/// bound of a range — the minimum experience required).
fn extract_years(text: &str) -> Option<u64> {
    static YEARS_RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"(?i)(\d{1,2})\s*(?:\+|\s*-\s*\d{1,2})?\s*(?:years|yrs)")
            .expect("static regex compiles")
    });
    YEARS_RE
        .captures(text)
        .and_then(|c| c[1].parse::<u64>().ok())
}

// ── skills ───────────────────────────────────────────────────────

/// Count of resume keywords mentioned in the JD, capped at 10 (a "full
/// match"), ×10. The alias table expands shorthands (`K8s` → `Kubernetes`).
const SKILLS_MATCH_CAP: usize = 10;

fn skills_score(
    raw_text: &str,
    resume_skills: &[String],
    aliases: &HashMap<String, String>,
) -> u64 {
    let jd = raw_text.to_lowercase();
    let matched = resume_skills
        .iter()
        .filter(|kw| keyword_matches(kw, &jd, aliases))
        .count();
    (matched.min(SKILLS_MATCH_CAP) * 100 / SKILLS_MATCH_CAP) as u64
}

/// A resume keyword matches the JD if its tokens appear (word-boundary,
/// case-insensitive). Parentheticals (`AWS (EC2, S3, …)`) match when the head
/// OR any parenthetical item appears. The alias table maps a shorthand to a
/// canonical keyword, so `K8s` in the JD matches the `Kubernetes` keyword.
fn keyword_matches(keyword: &str, jd_lower: &str, aliases: &HashMap<String, String>) -> bool {
    let kw = keyword.to_lowercase();
    if let Some(open) = kw.find('(') {
        let head = kw[..open].trim();
        let inner = kw[open + 1..].trim_end_matches(')');
        let alternatives: Vec<&str> = inner.split(',').map(str::trim).collect();
        return word_in(head, jd_lower) || alternatives.iter().any(|a| word_in(a, jd_lower));
    }
    let tokens: Vec<&str> = kw
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() >= 2)
        .collect();
    let token_match = !tokens.is_empty() && tokens.iter().all(|t| word_in(t, jd_lower));
    let alias_match = aliases.iter().any(|(alias, canonical)| {
        canonical.eq_ignore_ascii_case(keyword) && word_in(&alias.to_lowercase(), jd_lower)
    });
    token_match || alias_match
}

/// Word-boundary, case-insensitive substring match.
fn word_in(word: &str, text: &str) -> bool {
    if word.is_empty() {
        return false;
    }
    let pattern = format!(r"(?i)\b{}\b", regex::escape(word));
    regex::Regex::new(&pattern)
        .map(|re| re.is_match(text))
        .unwrap_or(false)
}

// ── compensation ─────────────────────────────────────────────────

/// Linear interpolation floor→ceiling; at/above ceiling = 100. `None` when
/// comp is unknown or floor/ceiling are unconfigured (the dimension drops
/// out).
fn compensation_score(config: &Config, extracted: &ExtractedFields) -> Option<u64> {
    let floor = config.compensation_floor?;
    let ceiling = config.compensation_ceiling?;
    let comp: &CompRange = extracted.comp.as_ref()?;
    // Compare against the top of the quoted range when present, else the
    // single/bottom figure (mirrors the gate's comparison).
    let value = comp.max.or(comp.min)?;
    if ceiling <= floor {
        return None;
    }
    let score = (value.saturating_sub(floor)) as f64 / (ceiling - floor) as f64 * 100.0;
    Some(score.clamp(0.0, 100.0).round() as u64)
}

// ── remote ───────────────────────────────────────────────────────

/// Confident remote = 100, unknown = 50, confident non-remote = 0 (defensive:
/// the remote-only gate normally rejects it before scoring).
fn remote_score(extracted: &ExtractedFields) -> u64 {
    match extracted.remote {
        Some(true) => 100,
        None => 50,
        Some(false) => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ScoringWeights;

    fn config() -> Config {
        Config {
            compensation_floor: Some(180_000),
            compensation_ceiling: Some(300_000),
            ..Default::default()
        }
    }

    fn extracted() -> ExtractedFields {
        ExtractedFields {
            title: Some("Staff Engineer".into()),
            company: Some("Acme".into()),
            remote: Some(true),
            comp: Some(CompRange {
                min: Some(220_000),
                max: Some(260_000),
                currency: "USD".into(),
                period: "year".into(),
                raw: "$220,000 - $260,000".into(),
            }),
            ..Default::default()
        }
    }

    // ── level ────────────────────────────────────────────────────

    #[test]
    fn level_from_title() {
        let mut e = extracted();
        for title in ["Principal Engineer", "Staff Engineer", "Architect"] {
            e.title = Some(title.into());
            assert_eq!(level_score(&e, "no years"), 100, "{title}");
        }
        for title in ["Senior Engineer", "Lead Engineer"] {
            e.title = Some(title.into());
            assert_eq!(level_score(&e, "no years"), 70, "{title}");
        }
        for title in [
            "Junior Engineer",
            "Associate Engineer",
            "Entry Level",
            "Intern",
        ] {
            e.title = Some(title.into());
            assert_eq!(level_score(&e, "no years"), 10, "{title}");
        }
        // Director is management, not an IC target — no level signal.
        e.title = Some("Director of Engineering".into());
        assert_eq!(level_score(&e, "no years"), 50);
    }

    #[test]
    fn level_from_years_fallback() {
        let mut e = extracted();
        e.title = Some("Engineer".into()); // no level signal
        assert_eq!(level_score(&e, "15+ years"), 100);
        assert_eq!(level_score(&e, "10 years experience"), 66);
        assert_eq!(level_score(&e, "5+ years"), 33);
        assert_eq!(level_score(&e, "no years"), 50);
    }

    #[test]
    fn level_title_keywords_match_on_word_boundaries() {
        // Regression: substring matching misclassifies unrelated titles —
        // "International" contains "intern", "Staffing" contains "staff".
        let mut e = extracted();
        for title in [
            "International Software Engineer",
            "Staffing Engineer",
            "Entryway Designer",
        ] {
            e.title = Some(title.into());
            assert_eq!(level_score(&e, "no years"), 50, "{title}");
        }
        // True positives keep working under boundary matching.
        for (title, expected) in [
            ("Staff Engineer", 100),
            ("Senior Engineer", 70),
            ("Junior Engineer", 10),
        ] {
            e.title = Some(title.into());
            assert_eq!(level_score(&e, "no years"), expected, "{title}");
        }
    }

    // ── skills ───────────────────────────────────────────────────

    #[test]
    fn skills_count_capped() {
        let skills = vec![
            "Kubernetes".to_string(),
            "Terraform".to_string(),
            "Rust".to_string(),
            "Python".to_string(),
        ];
        let jd = "We use Kubernetes and Terraform heavily.";
        assert_eq!(skills_score(jd, &skills, &HashMap::new()), 20); // 2 matches × 10
    }

    #[test]
    fn skills_caps_at_full_match() {
        let skills: Vec<String> = (0..12).map(|i| format!("skill{i}")).collect();
        let jd = skills.join(" ");
        assert_eq!(skills_score(&jd, &skills, &HashMap::new()), 100);
    }

    #[test]
    fn skills_alias_expands_shorthand() {
        let skills = vec!["Kubernetes".to_string()];
        let mut aliases = HashMap::new();
        aliases.insert("K8s".to_string(), "Kubernetes".to_string());
        assert_eq!(skills_score("We run K8s in prod.", &skills, &aliases), 10);
        assert_eq!(skills_score("We run containers.", &skills, &aliases), 0);
    }

    #[test]
    fn skills_parenthetical_matches_head_or_item() {
        let skills = vec!["AWS (EC2, S3, EKS, Kinesis, Lambda)".to_string()];
        assert_eq!(
            skills_score("We use AWS heavily.", &skills, &HashMap::new()),
            10
        );
        assert_eq!(
            skills_score("We use S3 for storage.", &skills, &HashMap::new()),
            10
        );
        assert_eq!(skills_score("We use Azure.", &skills, &HashMap::new()), 0);
    }

    #[test]
    fn skills_word_boundary_avoids_substring_false_positives() {
        let skills = vec!["Go".to_string()];
        // "Go" must not match "Google" or "cargo".
        assert_eq!(
            skills_score("We write Go services.", &skills, &HashMap::new()),
            10
        );
        assert_eq!(
            skills_score("We use Google Cloud.", &skills, &HashMap::new()),
            0
        );
    }

    #[test]
    fn skills_punctuated_keyword_matches() {
        // Regression: "C++" tokenizes to a single one-character token, which
        // the len >= 2 filter drops — so the keyword can never match.
        assert_eq!(
            skills_score("We use C++ heavily.", &["C++".to_string()], &HashMap::new()),
            10
        );
        // Guard: the boundary fix must not over-match — plain C is not C++.
        assert_eq!(
            skills_score("We use C only.", &["C++".to_string()], &HashMap::new()),
            0
        );
    }

    #[test]
    fn skills_single_character_keyword_matches() {
        // Regression: single-character tokens are dropped by the len >= 2
        // filter, so skills like "R" can never match.
        assert_eq!(
            skills_score(
                "Expert in R programming.",
                &["R".to_string()],
                &HashMap::new()
            ),
            10
        );
        // Guard: "R" must not match "Rust".
        assert_eq!(
            skills_score("We use Rust.", &["R".to_string()], &HashMap::new()),
            0
        );
    }

    #[test]
    fn skills_alias_with_punctuation_expands() {
        // Regression: `\b` cannot hold at the punctuation edges of ".NET" or
        // "C++", so aliases shaped like these never match.
        let aliases = HashMap::from([(".NET".to_string(), "DotNet".to_string())]);
        assert_eq!(
            skills_score("We use .NET services.", &["DotNet".to_string()], &aliases),
            10
        );
        let aliases = HashMap::from([("C++".to_string(), "Cpp".to_string())]);
        assert_eq!(
            skills_score("Heavy C++ workloads.", &["Cpp".to_string()], &aliases),
            10
        );
    }

    // ── compensation ─────────────────────────────────────────────

    #[test]
    fn compensation_linear_interpolation() {
        let mut e = extracted();
        e.comp = Some(CompRange {
            min: Some(180_000),
            max: Some(180_000),
            currency: "USD".into(),
            period: "year".into(),
            raw: "$180,000".into(),
        });
        assert_eq!(compensation_score(&config(), &e), Some(0));
        e.comp.as_mut().unwrap().max = Some(300_000);
        assert_eq!(compensation_score(&config(), &e), Some(100));
        e.comp.as_mut().unwrap().max = Some(240_000);
        assert_eq!(compensation_score(&config(), &e), Some(50));
    }

    #[test]
    fn compensation_unknown_or_unconfigured_drops_out() {
        let mut e = extracted();
        e.comp = None;
        assert_eq!(compensation_score(&config(), &e), None);
        let mut cfg = config();
        cfg.compensation_ceiling = None;
        assert_eq!(compensation_score(&cfg, &extracted()), None);
    }

    #[test]
    fn compensation_falls_back_to_min_when_max_absent() {
        // Mirrors the gate: a single-figure posting (max absent) scores
        // against min. (240k−180k)/(300k−180k) = 50.
        let mut e = extracted();
        e.comp = Some(CompRange {
            min: Some(240_000),
            max: None,
            currency: "USD".into(),
            period: "year".into(),
            raw: "$240,000".into(),
        });
        assert_eq!(compensation_score(&config(), &e), Some(50));
    }

    // ── remote ───────────────────────────────────────────────────

    #[test]
    fn remote_scores() {
        let mut e = extracted();
        e.remote = Some(true);
        assert_eq!(remote_score(&e), 100);
        e.remote = None;
        assert_eq!(remote_score(&e), 50);
        e.remote = Some(false);
        assert_eq!(remote_score(&e), 0);
    }

    // ── composite + breakdown ────────────────────────────────────

    #[test]
    fn composite_renormalizes_when_comp_unknown() {
        let mut e = extracted();
        e.comp = None;
        let result = score(
            &config(),
            &e,
            "Kubernetes experience",
            &["Kubernetes".to_string()],
        );
        // level 100 (staff), skills 10 (1 match), remote 100; comp drops out.
        // Equal weights → renormalized to 1/3 each → composite 70.
        assert_eq!(result.composite, 70);
        assert!(result.breakdown.contains("compensation: unknown"));
        assert_eq!(result.dimensions.len(), 3);
    }

    #[test]
    fn breakdown_shows_renormalized_weights() {
        let result = score(
            &config(),
            &extracted(),
            "5+ years",
            &["Kubernetes".to_string()],
        );
        // All four dimensions present, equal weights → 0.25 each.
        assert!(result.breakdown.contains("0.25·level"));
        assert!(result.breakdown.contains("0.25·remote"));
    }

    #[test]
    fn fmt_weight_trims() {
        assert_eq!(fmt_weight(0.3), "0.3");
        assert_eq!(fmt_weight(0.25), "0.25");
        assert_eq!(fmt_weight(0.5), "0.5");
        assert_eq!(fmt_weight(1.0), "1");
        assert_eq!(fmt_weight(0.333), "0.333");
    }

    #[test]
    fn degenerate_weights_do_not_break_the_contract() {
        // Regression: all-zero weights divide by zero — the breakdown prints
        // NaN and the composite silently leaves the documented 0–100 range.
        let cfg = Config {
            scoring_weights: ScoringWeights {
                level: 0.0,
                skills: 0.0,
                compensation: 0.0,
                remote: 0.0,
            },
            ..Default::default()
        };
        let result = score(
            &cfg,
            &extracted(),
            "Kubernetes experience",
            &["Kubernetes".to_string()],
        );
        assert!(
            !result.breakdown.contains("NaN"),
            "breakdown: {}",
            result.breakdown
        );
        assert!(result.composite <= 100, "composite: {}", result.composite);
    }
}
