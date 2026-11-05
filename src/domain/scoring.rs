//! Scoring (spec §3, design doc 0001 §3 `scored` event). Fully deterministic:
//! per-dimension 0–100 scores, each with a `confidence` field (default 1.0;
//! only meaningful once LLM scorers arrive), combined into a weighted-sum
//! composite with a human-readable breakdown.
//!
//! Dimensions: `level`, `skills`, `compensation`, `remote`. A dimension that
//! cannot be scored (unknown comp, no resume) drops out of the composite with
//! weight renormalization, and the breakdown notes it.

use std::collections::HashMap;

use tracing::warn;

use crate::{
    config::Config,
    domain::{
        cached_regex,
        events::{DimensionScore, ExtractedFields},
    },
};

/// The result of scoring a posting that passed all gates. `revision` is not
/// here — the aggregate fills it in (it counts gate/score evaluations).
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct ScoreResult {
    pub composite: u64,
    pub dimensions: Vec<DimensionScore>,
    pub breakdown: String,
}

/// A dimension that could not be scored, and why — the reason reaches the
/// breakdown so the operator can tell an absent input from a distrusted one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DroppedReason {
    /// No input to score (e.g. the posting quotes no compensation).
    Unknown,
    /// The input was present but is not credible (see [`CompVerdict`]).
    Implausible { value: u64, cap: u64 },
    /// No resume configured, so skills cannot be matched.
    NoResume,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DroppedDimension {
    name: &'static str,
    reason: DroppedReason,
}

/// What scoring decided about a posting's compensation range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CompVerdict {
    /// A plausible range, scored linearly across floor→ceiling.
    Scored(u64),
    /// Nothing to score (absent range, or no floor/ceiling configured).
    Unknown,
    /// A range that exists but is not credible — scored as unknown, and named
    /// in the breakdown so it does not read as a simple absence.
    Implausible { value: u64, cap: u64 },
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
    let mut dropped: Vec<DroppedDimension> = Vec::new();

    dims.push(("level", level_score(extracted, raw_text)));

    if resume_skills.is_empty() {
        dropped.push(DroppedDimension {
            name: "skills",
            reason: DroppedReason::NoResume,
        });
    } else {
        // Skills match against the title too: API-sourced postings may
        // mention a skill only in the title, not the extracted body.
        let skills_text = match extracted.title.as_deref() {
            Some(title) => format!("{title}\n{raw_text}"),
            None => raw_text.to_string(),
        };
        dims.push((
            "skills",
            skills_score(&skills_text, resume_skills, &config.aliases),
        ));
    }

    match compensation_score(config, extracted) {
        CompVerdict::Scored(s) => dims.push(("compensation", s)),
        CompVerdict::Unknown => dropped.push(DroppedDimension {
            name: "compensation",
            reason: DroppedReason::Unknown,
        }),
        CompVerdict::Implausible { value, cap } => {
            // The breakdown is the always-visible signal (the file sink filters
            // `warn` at the default level — see `env_filter`/decision 0005);
            // this line is for operators who raise the level.
            warn!(
                company = extracted.company.as_deref().unwrap_or_default(),
                title = extracted.title.as_deref().unwrap_or_default(),
                value,
                cap,
                multiplier = config.plausibility_multiplier(),
                quoted = extracted
                    .comp
                    .as_ref()
                    .map(|c| c.raw.as_str())
                    .unwrap_or_default(),
                "compensation implausible; dimension dropped"
            );
            dropped.push(DroppedDimension {
                name: "compensation",
                reason: DroppedReason::Implausible { value, cap },
            });
        }
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
    dropped: &[DroppedDimension],
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
            // Guard against a zero total weight (all-zero config): the
            // renormalized weight is 0, not NaN.
            let renormalized = if total_weight > 0.0 {
                d.weight / total_weight
            } else {
                0.0
            };
            format!("{}·{}({})", fmt_weight(renormalized), d.name, d.score)
        })
        .collect();
    let mut breakdown = format!("{composite} = {}", parts.join(" + "));
    if !dropped.is_empty() {
        let notes: Vec<String> = dropped
            .iter()
            .map(|d| match d.reason {
                DroppedReason::Unknown => {
                    format!("{}: unknown, weight renormalized", d.name)
                }
                DroppedReason::Implausible { value, cap } => format!(
                    "{}: implausible ({value} exceeds cap {cap}), weight renormalized",
                    d.name
                ),
                DroppedReason::NoResume => format!("{}: no resume, weight renormalized", d.name),
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
        .any(|k| token_in(k, &title))
    {
        return 100;
    }
    if ["senior", "lead"].iter().any(|k| token_in(k, &title)) {
        return 70;
    }
    if ["junior", "associate", "entry", "intern"]
        .iter()
        .any(|k| token_in(k, &title))
    {
        return 10;
    }
    // No title signal: fall back to quoted years (15+ = 100), else unknown.
    if let Some(years) = extract_years(raw_text) {
        return years.min(15) * 100 / 15;
    }
    50
}

/// Estimate required years of experience from "X years" mentions.
///
/// A posting can mention years in non-experience contexts ("2 years of
/// Kubernetes", "we've been in business for 20 years"). The word
/// "experience" (or "exp") after a "years" mention is the signal that the
/// years refer to the role's experience requirement, so we prefer the
/// mention(s) followed by "experience" before the next "years" mention and
/// take the max of those (the highest requirement is the best level signal).
/// When no mention is experience-adjacent, we fall back to the first mention
/// (the bare "5+ years" shorthand). A range ("5–10 years") still yields its
/// lower bound — the minimum experience required. Known limitation: the
/// fallback can still return a non-experience mention (e.g. a company-age
/// "we've been in business for 20 years" with no other years mention) — a
/// false positive accepted for v0 (the level dimension is a fallback anyway).
fn extract_years(text: &str) -> Option<u64> {
    static YEARS_RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"(?i)(\d{1,2})\s*(?:\+|\s*[-–—]\s*\d{1,2})?\s*(?:years|yrs)")
            .expect("static regex compiles")
    });
    static EXPERIENCE_RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"(?i)\b(?:experience|exp)\b").expect("static regex compiles")
    });

    // (years, start, end) for every "X years" mention, in text order.
    let mentions: Vec<(u64, usize, usize)> = YEARS_RE
        .captures_iter(text)
        .filter_map(|c| {
            let m = c.get(0)?;
            let years = c[1].parse::<u64>().ok()?;
            Some((years, m.start(), m.end()))
        })
        .collect();

    let first = mentions.first().map(|(years, _, _)| *years)?;

    // A mention is experience-adjacent when "experience"/"exp" appears after
    // it but before the next "years" mention (or within a short window when
    // it is the last mention). This keeps "2 years of Kubernetes" from
    // borrowing the "experience" that belongs to a later "10+ years overall
    // experience".
    const WINDOW: usize = 40;
    let adjacent: Vec<u64> = mentions
        .iter()
        .enumerate()
        .filter_map(|(i, (years, _, end))| {
            let limit = mentions
                .get(i + 1)
                .map(|(_, next_start, _)| *next_start)
                .unwrap_or_else(|| text.ceil_char_boundary((*end + WINDOW).min(text.len())));
            EXPERIENCE_RE.is_match(&text[*end..limit]).then_some(*years)
        })
        .collect();

    adjacent.into_iter().max().or(Some(first))
}

// ── skills ───────────────────────────────────────────────────────

/// Count of resume keywords mentioned in the JD, capped at 10 (a "full
/// match"), ×10. The alias table expands shorthands (`K8s` → `Kubernetes`).
const SKILLS_MATCH_CAP: usize = 10;

fn skills_score(text: &str, resume_skills: &[String], aliases: &HashMap<String, String>) -> u64 {
    let jd = text.to_lowercase();
    let matched = resume_skills
        .iter()
        .filter(|kw| keyword_matches(kw, &jd, aliases))
        .count();
    (matched.min(SKILLS_MATCH_CAP) * 100 / SKILLS_MATCH_CAP) as u64
}

/// A resume keyword matches the JD if its tokens appear (token-boundary,
/// case-insensitive). Parentheticals (`AWS (EC2, S3, …)`) match when the head
/// OR any parenthetical item appears. The alias table maps a shorthand to a
/// canonical keyword, so `K8s` in the JD matches the `Kubernetes` keyword.
fn keyword_matches(keyword: &str, jd_lower: &str, aliases: &HashMap<String, String>) -> bool {
    let kw = keyword.to_lowercase();
    if let Some(open) = kw.find('(') {
        let head = kw[..open].trim();
        let inner = kw[open + 1..].trim_end_matches(')');
        let alternatives: Vec<&str> = inner.split(',').map(str::trim).collect();
        return token_in(head, jd_lower) || alternatives.iter().any(|a| token_in(a, jd_lower));
    }
    // Split on whitespace (not all non-alphanumerics) so punctuated keywords
    // like `C++` and `.NET` stay whole, and single-character keywords (`R`,
    // `C`) are preserved.
    let tokens: Vec<&str> = kw.split_whitespace().collect();
    let token_match = !tokens.is_empty() && tokens.iter().all(|t| token_in(t, jd_lower));
    let alias_match = aliases.iter().any(|(alias, canonical)| {
        canonical.eq_ignore_ascii_case(keyword) && token_in(&alias.to_lowercase(), jd_lower)
    });
    token_match || alias_match
}

/// Token match with alphanumeric-aware boundaries (case-insensitive). Unlike
/// `\b`, this holds at punctuation edges, so single-character and punctuated
/// keywords (`R`, `C++`, `.NET`) match correctly without over-matching (`R`
/// must not match `Rust`, `C++` must not match `C`).
///
/// Known over-match (accepted): a single-character keyword (`C`) matches a
/// punctuation-adjacent token (`C++`), because `+` is a non-alphanumeric
/// boundary. Accepted — the resume's single-character skills are rare, and a
/// false positive costs one glance while a false negative is invisible.
fn token_in(token: &str, text: &str) -> bool {
    if token.is_empty() {
        return false;
    }
    // The regex crate does not support look-around, so the boundaries are an
    // explicit group: the token must be preceded and followed by a
    // non-alphanumeric character (or the start/end of the text). Compiled
    // once per token (cached_regex), not per call.
    let re = cached_regex(token, || {
        format!(
            r"(?i)(?:^|[^[:alnum:]]){}(?:$|[^[:alnum:]])",
            regex::escape(token)
        )
    });
    re.is_match(text)
}

// ── compensation ─────────────────────────────────────────────────

/// Linear interpolation floor→ceiling; at/above ceiling = 100. [`CompVerdict::Unknown`]
/// when comp is unknown or floor/ceiling are unconfigured, and
/// [`CompVerdict::Implausible`] when the quoted value exceeds the plausibility
/// cap (ceiling × `compensation_plausibility_multiplier`) — an upstream unit or
/// currency error must not saturate the dimension at 100 and inflate the
/// composite. An implausible range is never a rejection: the permissive-gate
/// philosophy still governs, and the dimension simply drops out.
fn compensation_score(config: &Config, extracted: &ExtractedFields) -> CompVerdict {
    let Some(floor) = config.compensation_floor else {
        return CompVerdict::Unknown;
    };
    let Some(ceiling) = config.compensation_ceiling else {
        return CompVerdict::Unknown;
    };
    let Some(comp) = extracted.comp.as_ref() else {
        return CompVerdict::Unknown;
    };
    // Inverted bounds are an upstream data error; normalize before judging.
    let Some(value) = comp.clone().normalized().compared_value() else {
        return CompVerdict::Unknown;
    };
    if let Some(cap) = config.plausibility_cap()
        && value > cap
    {
        return CompVerdict::Implausible { value, cap };
    }
    if ceiling <= floor {
        return CompVerdict::Unknown;
    }
    let score = (value.saturating_sub(floor)) as f64 / (ceiling - floor) as f64 * 100.0;
    CompVerdict::Scored(score.clamp(0.0, 100.0).round() as u64)
}

// ── remote ───────────────────────────────────────────────────────

/// Confident remote = 100, unknown = 50, confident non-remote = 0. The
/// remote-only gate rejects non-remote only when `remote_only` is enabled
/// (default false); with a stock config, non-remote leads reach scoring and
/// are downranked (0) rather than rejected.
fn remote_score(extracted: &ExtractedFields) -> u64 {
    match extracted.remote {
        Some(true) => 100,
        None => 50,
        Some(false) => 0,
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::{config::ScoringWeights, domain::events::CompRange};

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
    fn years_range_with_typographic_dashes_uses_lower_bound() {
        // Regression: the years regex only knows ASCII '-', so an en/em-dash
        // range skips the range group and the search falls through to the
        // UPPER bound ("5–10 years" matches "10 years"). The documented
        // contract is the lower bound — the minimum experience required.
        let mut e = extracted();
        e.title = Some("Engineer".into()); // no level signal
        assert_eq!(level_score(&e, "5–10 years of experience"), 33);
        assert_eq!(level_score(&e, "5—10 years of experience"), 33);
    }

    #[test]
    fn extract_years_prefers_experience_adjacent_mention() {
        // review round 2 (PR #7): leftmost-match is arbitrary. "2 years
        // of Kubernetes" is a technology mention, not the role's experience
        // requirement; "10+ years overall experience" is.
        assert_eq!(
            extract_years("2 years of Kubernetes; 10+ years overall experience"),
            Some(10)
        );
    }

    #[test]
    fn extract_years_ignores_company_age_mentions() {
        // "20 years in business" is the company's age, not a requirement.
        assert_eq!(
            extract_years("5 years of experience; we've been in business for 20 years"),
            Some(5)
        );
    }

    #[test]
    fn extract_years_takes_max_of_experience_mentions() {
        // Both mentions are experience-adjacent; the higher requirement wins.
        assert_eq!(
            extract_years("2 years of Kubernetes experience; 10+ years overall experience"),
            Some(10)
        );
    }

    #[test]
    fn extract_years_falls_back_to_first_mention() {
        // No "experience" anywhere: the bare "5+ years" shorthand.
        assert_eq!(extract_years("5+ years"), Some(5));
    }

    #[test]
    fn level_prefers_overall_experience_over_tech_years() {
        // End-to-end: the level dimension must not let a technology-specific
        // years mention outrank the overall experience requirement.
        let mut e = extracted();
        e.title = Some("Engineer".into()); // no level signal
        assert_eq!(
            level_score(&e, "2 years of Kubernetes; 10+ years overall experience"),
            66
        );
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
    fn single_char_keyword_over_matches_punctuated_token() {
        // Accepted over-match (review): a single-character keyword (`C`)
        // matches a punctuation-adjacent token (`C++`), because `+` is a
        // non-alphanumeric boundary. Pinned so the behavior is explicit, not
        // drift.
        assert_eq!(
            skills_score("We use C++ heavily.", &["C".to_string()], &HashMap::new()),
            10
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
        assert_eq!(compensation_score(&config(), &e), CompVerdict::Scored(0));
        e.comp.as_mut().unwrap().max = Some(300_000);
        assert_eq!(compensation_score(&config(), &e), CompVerdict::Scored(100));
        e.comp.as_mut().unwrap().max = Some(240_000);
        assert_eq!(compensation_score(&config(), &e), CompVerdict::Scored(50));
    }

    #[test]
    fn compensation_unknown_or_unconfigured_drops_out() {
        let mut e = extracted();
        e.comp = None;
        assert_eq!(compensation_score(&config(), &e), CompVerdict::Unknown);
        let mut cfg = config();
        cfg.compensation_ceiling = None;
        assert_eq!(compensation_score(&cfg, &extracted()), CompVerdict::Unknown);
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
        assert_eq!(compensation_score(&config(), &e), CompVerdict::Scored(50));
    }

    // ── implausible compensation (comp-plausibility, GWLJ-f5pdka) ─

    fn comp_range(min: u64, max: u64) -> CompRange {
        CompRange {
            min: Some(min),
            max: Some(max),
            currency: "USD".into(),
            period: "year".into(),
            raw: format!("${min} - ${max}"),
        }
    }

    fn with_comp(min: u64, max: u64) -> ExtractedFields {
        let mut e = extracted();
        e.comp = Some(comp_range(min, max));
        e
    }

    #[test]
    fn implausible_ranges_drop_the_dimension() {
        // The five real upstream errors from the 2026-10-28 paid run. The
        // default config (floor 180k, ceiling 300k) puts the cap at 600_000.
        for (min, max, value) in [
            (187_200_000u64, 208_000_000u64, 208_000_000u64), // Advantech PM
            (218_400_000, 301_600_000, 301_600_000),          // Velociti QC lead
            (176_800_000, 185_120_000, 185_120_000),          // Mayfair accountant
            (2_080_000, 2_808_000, 2_808_000),                // "$40k-$54k a week" x52
            (1_560_000, 2_280_000, 2_280_000),                // WW Talents SWE
        ] {
            assert_eq!(
                compensation_score(&config(), &with_comp(min, max)),
                CompVerdict::Implausible {
                    value,
                    cap: 600_000
                },
                "range {min}-{max} should be implausible"
            );
        }
    }

    #[test]
    fn plausible_high_value_still_scores() {
        // Above the ceiling but within the cap: saturates at 100, as today.
        assert_eq!(
            compensation_score(&config(), &with_comp(300_000, 350_000)),
            CompVerdict::Scored(100)
        );
    }

    #[test]
    fn at_the_cap_scores_normally() {
        // The rule is strict "exceeds": a value exactly at the cap scores.
        assert_eq!(
            compensation_score(&config(), &with_comp(500_000, 600_000)),
            CompVerdict::Scored(100)
        );
    }

    #[test]
    fn raised_multiplier_admits_a_higher_range() {
        let mut cfg = config();
        cfg.compensation_plausibility_multiplier = Some(3.0);
        assert_eq!(
            compensation_score(&cfg, &with_comp(650_000, 700_000)),
            CompVerdict::Scored(100)
        );
        assert_eq!(cfg.plausibility_cap(), Some(900_000));
    }

    #[test]
    fn inverted_range_is_normalized_before_judging() {
        // min above max is an upstream error; the top of the range (the
        // larger figure) is what gets compared, so this is plausible.
        let inverted = with_comp(400_000, 250_000);
        assert_eq!(
            compensation_score(&config(), &inverted),
            CompVerdict::Scored(100)
        );
        let normalized = inverted.comp.clone().unwrap().normalized();
        assert_eq!(
            (normalized.min, normalized.max),
            (Some(250_000), Some(400_000))
        );
        // `raw` is the upstream presentation string and is deliberately left
        // alone: normalization is about comparison, not about rewriting history.
        assert_eq!(normalized.raw, "$400000 - $250000");
    }

    #[test]
    fn breakdown_names_the_implausible_value_and_cap() {
        let skills = vec!["Kubernetes".to_string()];
        let result = score(
            &config(),
            &with_comp(187_200_000, 208_000_000),
            "body",
            &skills,
        );
        assert!(
            result
                .breakdown
                .contains("compensation: implausible (208000000 exceeds cap 600000)"),
            "unexpected breakdown: {}",
            result.breakdown
        );
        // The dimension is absent, not merely zero.
        assert!(!result.dimensions.iter().any(|d| d.name == "compensation"));
    }

    #[test]
    fn implausible_comp_scores_like_absent_comp() {
        let skills = vec!["Kubernetes".to_string()];
        let mut absent = extracted();
        absent.comp = None;
        let with_absent = score(&config(), &absent, "body", &skills);
        let with_implausible = score(
            &config(),
            &with_comp(187_200_000, 208_000_000),
            "body",
            &skills,
        );
        assert_eq!(with_absent.composite, with_implausible.composite);
        assert_eq!(with_absent.dimensions, with_implausible.dimensions);
        // Only the explanation differs.
        assert_ne!(with_absent.breakdown, with_implausible.breakdown);
    }

    proptest! {
        /// Anything above the cap is judged implausible, whatever the range's
        /// width; at or below the cap, nothing is.
        #[test]
        fn the_cap_is_the_boundary(
            cap_excess in 1u64..100_000_000u64,
            width in 0u64..1_000_000u64,
        ) {
            let value = 600_000 + cap_excess;
            let above = compensation_score(
                &config(),
                &with_comp(value.saturating_sub(width), value),
            );
            prop_assert_eq!(above, CompVerdict::Implausible { value, cap: 600_000 });

            let within = compensation_score(&config(), &with_comp(value - cap_excess, value - cap_excess));
            let within_is_implausible = matches!(within, CompVerdict::Implausible { .. });
            prop_assert!(!within_is_implausible);
        }

        /// Within [floor, cap] the dimension is monotone non-decreasing
        /// (reaching 100 at the ceiling and staying there).
        #[test]
        fn dimension_is_monotone_up_to_the_cap(
            a in 180_000u64..600_000u64,
            delta in 0u64..420_000u64,
        ) {
            let b = (a + delta).min(600_000);
            let CompVerdict::Scored(sa) = compensation_score(&config(), &with_comp(a, a)) else {
                prop_assert!(false, "expected a score for {a}");
                unreachable!()
            };
            let CompVerdict::Scored(sb) = compensation_score(&config(), &with_comp(b, b)) else {
                prop_assert!(false, "expected a score for {b}");
                unreachable!()
            };
            prop_assert!(sa <= sb, "{sa} > {sb} for {a} -> {b}");
        }

        /// Normalization is symmetric: swapping inverted bounds yields the
        /// same normalized range either way round.
        #[test]
        fn normalization_is_symmetric(lo in 0u64..1_000_000u64, hi in 0u64..1_000_000u64) {
            let a = CompRange {
                min: Some(lo),
                max: Some(hi),
                currency: "USD".into(),
                period: "year".into(),
                raw: String::new(),
            };
            let b = CompRange {
                min: Some(hi),
                max: Some(lo),
                currency: "USD".into(),
                period: "year".into(),
                raw: String::new(),
            };
            prop_assert_eq!(a.normalized(), b.normalized());
        }
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
    fn non_uniform_weights_renormalize_with_dropped_dimension() {
        // The non-default weighted-sum path: unequal weights, one dimension
        // dropped, renormalized breakdown.
        let mut cfg = config();
        cfg.scoring_weights = ScoringWeights {
            level: 0.5,
            skills: 0.5,
            compensation: 0.0,
            remote: 0.0,
        };
        let mut e = extracted();
        e.comp = None;
        let result = score(
            &cfg,
            &e,
            "Kubernetes experience",
            &["Kubernetes".to_string()],
        );
        // level 100 (staff), skills 10 (1 match), remote 100; comp drops out.
        // Weights level 0.5, skills 0.5, remote 0.0 → total 1.0 → composite
        // (0.5·100 + 0.5·10 + 0.0·100) = 55.
        assert_eq!(result.composite, 55);
        assert!(result.breakdown.contains("0.5·level(100)"));
        assert!(result.breakdown.contains("0.5·skills(10)"));
        assert!(result.breakdown.contains("compensation: unknown"));
    }

    #[test]
    fn fmt_weight_trims() {
        assert_eq!(fmt_weight(0.3), "0.3");
        assert_eq!(fmt_weight(0.25), "0.25");
        assert_eq!(fmt_weight(0.5), "0.5");
        assert_eq!(fmt_weight(1.0), "1");
        assert_eq!(fmt_weight(0.333), "0.333");
        assert_eq!(fmt_weight(0.0), "0");
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
    // ── observability: the implausibility warn ───────────────────

    /// Capture what the `warn!` writes, without a global subscriber (tests run
    /// in parallel, so the subscriber is scoped to the closure).
    fn captured_warnings(body: impl FnOnce()) -> String {
        use std::sync::{Arc, Mutex};

        use tracing_subscriber::layer::SubscriberExt as _;

        #[derive(Clone)]
        struct Buffer(Arc<Mutex<Vec<u8>>>);

        impl std::io::Write for Buffer {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let sink = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry()
            .with(tracing_subscriber::filter::LevelFilter::WARN)
            .with(
                tracing_subscriber::fmt::layer()
                    .with_ansi(false)
                    .with_writer({
                        let sink = sink.clone();
                        move || Buffer(sink.clone())
                    }),
            );
        tracing::subscriber::with_default(subscriber, body);
        String::from_utf8(sink.lock().unwrap().clone()).unwrap_or_default()
    }

    #[test]
    fn implausible_comp_warns_once_with_identifying_fields() {
        let captured = captured_warnings(|| {
            let mut e = with_comp(187_200_000, 208_000_000);
            e.company = Some("Advantech USA".into());
            e.title = Some("Product Manager".into());
            let _ = score(&config(), &e, "body", &["Kubernetes".to_string()]);
        });
        assert!(captured.contains("compensation implausible"), "{captured}");
        // The offending input must be locatable from the line alone.
        for needle in [
            "Advantech USA",
            "Product Manager",
            "value=208000000",
            "cap=600000",
            "multiplier=2",
            "$187200000",
        ] {
            assert!(captured.contains(needle), "missing {needle} in: {captured}");
        }
        assert_eq!(captured.matches("compensation implausible").count(), 1);
    }

    #[test]
    fn plausible_comp_warns_nothing() {
        let captured = captured_warnings(|| {
            let _ = score(&config(), &with_comp(300_000, 350_000), "body", &[]);
        });
        assert!(captured.is_empty(), "unexpected log output: {captured}");
    }
}
