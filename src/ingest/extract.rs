//! Best-effort, deterministic field extraction from posting text and HTML.
//!
//! Comp coverage is spotty everywhere (APIs and sites alike); these
//! heuristics take what exists and leave the rest absent.

use std::sync::LazyLock;

use dom_smoothie::Readability;
use miette::Result;
use regex::Regex;
use url::Url;

use crate::domain::events::{CompRange, ExtractedFields};

// Static patterns compile once via LazyLock — the deref cannot fail, which
// removes the per-call `unwrap()` entirely (and the recompilation).
static REMOTE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bremote\b").expect("static regex compiles"));
static NEGATED_REMOTE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(not|no|never|isn't|isnt|aren't|arent)\s+(?:a\s+|an\s+)?remote\b\w*|\bnon-remote\b|\bremote\s+is\s+not\b",
    )
    .expect("static regex compiles")
});
static ONSITE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\bhybrid\b|\bon[- ]site\b|\bin[- ]office\b|\bmust be (located|based) in\b")
        .expect("static regex compiles")
});
static MASKED_ONSITE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(not|no|never|without|instead of|rather than)\s+(?:a\s+|an\s+|any\s+)?(hybrid|on[- ]site|in[- ]office)\b\w*(\s+(requirement|policy|option))?",
    )
    .expect("static regex compiles")
});
static COMP_RANGE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)(?:USD\s*)?\$\s*(\d{2,3}(?:,\d{3})+|\d+(?:\.\d+)?)\s*(k)?\s*(?:-|–|—|\bto\b)\s*(?:USD\s*)?\$?\s*(\d{2,3}(?:,\d{3})+|\d+(?:\.\d+)?)\s*(k)?",
    )
    .expect("static regex compiles")
});
static COMP_SINGLE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:USD\s*)?\$\s*(\d{2,3}(?:,\d{3})+)\s*(k)?").expect("static regex compiles")
});
static HOURLY_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(per\s+hour|hourly|/hr|an\s+hour)\b").expect("static regex compiles")
});
static YEARLY_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(annually|yearly|per\s+year|/yr|a\s+year)\b").expect("static regex compiles")
});
static REQ_ID_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(?:req(?:uisition)?(?:\s*id)?|job\s*(?:id|number|requisition)|requisition\s*id)\s*[:#]?\s*([A-Z0-9][A-Z0-9-]{2,})\b",
    )
    .expect("static regex compiles")
});
static JSON_LD_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?is)<script[^>]*type\s*=\s*["']application/ld\+json["'][^>]*>(.*?)</script>"#)
        .expect("static regex compiles")
});

/// Convert an HTML fragment (e.g. an API-returned description body) to
/// plain text for `raw_text` and regex extraction.
pub fn html_to_text(html: &str) -> String {
    html2text::from_read(html.as_bytes(), 100).unwrap_or_else(|_| html.to_string())
}

/// Main-content extraction for full HTML pages (unknown sites, the
/// fallback path). Returns `(title, body_text)`.
pub fn extract_main_text(html: &str, url: &Url) -> Result<(Option<String>, String)> {
    let mut readability = Readability::new(html, Some(url.as_str()), None)
        .map_err(|e| miette::miette!("readability failed: {e}"))?;
    let article = readability
        .parse()
        .map_err(|e| miette::miette!("readability parse failed: {e}"))?;
    let title = if article.title.trim().is_empty() {
        None
    } else {
        Some(article.title.trim().to_string())
    };
    Ok((title, html_to_text(&article.content)))
}

/// A structured job posting extracted from JSON-LD (`<script
/// type="application/ld+json">`). Many boards embed this for SEO even when
/// the visible page is JS-rendered (Ashby, and others).
pub struct JsonLdJobPosting {
    pub title: String,
    pub description_html: String,
    pub company: Option<String>,
    pub location: Option<String>,
    pub req_id: Option<String>,
    pub remote: Option<bool>,
}

/// Extract a `JobPosting` from any JSON-LD script tag in the page. Returns
/// `None` when no usable posting is found (caller falls back to readability).
pub fn extract_json_ld(html: &str) -> Option<JsonLdJobPosting> {
    for caps in JSON_LD_RE.captures_iter(html) {
        let raw = caps.get(1)?.as_str();
        let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) else {
            continue;
        };
        if let Some(job) = find_job_posting(&value) {
            return Some(job);
        }
    }
    None
}

fn find_job_posting(value: &serde_json::Value) -> Option<JsonLdJobPosting> {
    // JSON-LD may be a single object, an array, or a `@graph` array.
    let candidates: Vec<&serde_json::Value> = match value {
        serde_json::Value::Array(items) => items.iter().collect(),
        serde_json::Value::Object(map) => {
            if let Some(graph) = map.get("@graph").and_then(serde_json::Value::as_array) {
                graph.iter().collect()
            } else {
                vec![value]
            }
        }
        _ => return None,
    };
    for candidate in candidates {
        if !is_job_posting(candidate) {
            continue;
        }
        let title = candidate.get("title").and_then(serde_json::Value::as_str)?;
        return Some(JsonLdJobPosting {
            title: title.to_string(),
            description_html: candidate
                .get("description")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string(),
            company: candidate
                .get("hiringOrganization")
                .and_then(|o| o.get("name"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
            location: extract_json_ld_location(candidate),
            req_id: candidate
                .get("identifier")
                .and_then(|i| i.get("value"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
            remote: extract_json_ld_remote(candidate),
        });
    }
    None
}

fn is_job_posting(value: &serde_json::Value) -> bool {
    match value.get("@type") {
        Some(serde_json::Value::String(s)) => s.eq_ignore_ascii_case("JobPosting"),
        Some(serde_json::Value::Array(items)) => items.iter().any(|i| {
            i.as_str()
                .is_some_and(|s| s.eq_ignore_ascii_case("JobPosting"))
        }),
        _ => false,
    }
}

fn extract_json_ld_location(job: &serde_json::Value) -> Option<String> {
    let loc = job.get("jobLocation")?;
    match loc {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Object(_) => {
            let addr = loc.get("address")?;
            match addr {
                serde_json::Value::String(s) => Some(s.clone()),
                serde_json::Value::Object(_) => {
                    let parts: Vec<&str> = ["addressLocality", "addressRegion", "addressCountry"]
                        .iter()
                        .filter_map(|k| addr.get(*k).and_then(serde_json::Value::as_str))
                        .collect();
                    if parts.is_empty() {
                        None
                    } else {
                        Some(parts.join(", "))
                    }
                }
                _ => None,
            }
        }
        _ => None,
    }
}

fn extract_json_ld_remote(job: &serde_json::Value) -> Option<bool> {
    let t = job
        .get("jobLocationType")
        .and_then(serde_json::Value::as_str)?;
    match t.to_ascii_uppercase().as_str() {
        "TELECOMMUTE" => Some(true),
        "ONSITE" | "HYBRID" => Some(false),
        _ => None,
    }
}

/// Best-effort structured fields from plain text (and optional location
/// string supplied by an API).
pub fn extract_fields(text: &str, location: Option<&str>) -> ExtractedFields {
    ExtractedFields {
        location: location.map(str::to_string),
        remote: detect_remote(location, text),
        comp: extract_comp(text),
        ..Default::default()
    }
}

/// Remote detection is a weak signal and we treat it that way (this feeds
/// the remote-only gate in Increment 2): a location string lacking
/// "remote" is not proof of on-site — postings can be remote and still
/// list a physical location — so the body is consulted before concluding
/// `Some(false)`.
fn detect_remote(location: Option<&str>, text: &str) -> Option<bool> {
    // Explicit work-arrangement negatives (confirmed design, 2026-08-26):
    // "hybrid", "on-site in X", "in-office", "must be located/based in"
    // are confident on-site signals and fail the gate even when the word
    // "remote" appears somewhere. Negated forms are masked first, so a
    // remote posting saying "not hybrid" or "no on-site requirement" is
    // NOT a confident negative (that would be an invisible false
    // rejection — the failure mode the gate policy exists to avoid).
    let head: String = text.chars().take(4000).collect();
    let head_masked = MASKED_ONSITE_RE.replace_all(&head, "");
    let location_text = location.unwrap_or_default();
    let location_masked = MASKED_ONSITE_RE.replace_all(location_text, "");
    if ONSITE_RE.is_match(&location_masked) || ONSITE_RE.is_match(&head_masked) {
        return Some(false);
    }
    let body_says_remote = REMOTE_RE.is_match(&NEGATED_REMOTE_RE.replace_all(&head, ""));
    match location {
        Some(loc) if loc.to_lowercase().contains("remote") => Some(true),
        Some(_) if body_says_remote => Some(true),
        // Location present but silent on arrangement: honestly unknown, not
        // a confident negative. The remote-only gate treats this case as
        // reject-or-pass by config toggle (`reject_location_only`, default
        // false — start permissive).
        Some(_) => None,
        None if body_says_remote => Some(true),
        None => None,
    }
}

/// Hours per year used to annualize hourly rates (40 h/wk × 52 wk).
/// Compensation is normalized to USD/year at the extraction edge: the
/// internal representation (and the event payload) is always a yearly
/// total, with `period` recording the source period detected. We roll the
/// multiplication ourselves rather than pulling in `rusty_money` for now —
/// the only arithmetic anywhere in v0 is this conversion and integer
/// floor/ceiling comparisons, and USD is the only currency handled;
/// integrating a money type is filed as a follow-up pebble.
const HOURS_PER_YEAR: u64 = 2080;

/// Salary range patterns: `$220,000 - $290,000`, `$220k–$290k`,
/// `USD 220,000 to 290,000`. Single amounts (`$180,000/yr`) set `min` only.
/// `min`/`max` are always USD/year (hourly rates are annualized).
pub fn extract_comp(text: &str) -> Option<CompRange> {
    if let Some(caps) = COMP_RANGE_RE.captures(text) {
        let m = caps
            .get(0)
            .expect("group 0 is present when captures succeeds");
        let period = detect_period_near(text, m.start(), m.end());
        let min = parse_amount(&caps[1], caps.get(2).map(|m| m.as_str()), &period);
        let max = parse_amount(&caps[3], caps.get(4).map(|m| m.as_str()), &period);
        if let (Some(min), Some(max)) = (min, max) {
            let raw = caps[0].trim().to_string();
            let (min, max) = annualize(min, max, &period);
            return Some(CompRange {
                min: Some(min),
                max: Some(max),
                currency: "USD".into(),
                period,
                raw,
            });
        }
    }

    if let Some(caps) = COMP_SINGLE_RE.captures(text) {
        let m = caps
            .get(0)
            .expect("group 0 is present when captures succeeds");
        let period = detect_period_near(text, m.start(), m.end());
        if let Some(amount) = parse_amount(&caps[1], caps.get(2).map(|m| m.as_str()), &period) {
            let raw = caps[0].trim().to_string();
            let (amount, _) = annualize(amount, amount, &period);
            return Some(CompRange {
                min: Some(amount),
                max: None,
                currency: "USD".into(),
                period,
                raw,
            });
        }
    }
    None
}

fn annualize(min: u64, max: u64, period: &str) -> (u64, u64) {
    if period == "hour" {
        (min * HOURS_PER_YEAR, max * HOURS_PER_YEAR)
    } else {
        (min, max)
    }
}

fn parse_amount(digits: &str, k_suffix: Option<&str>, period: &str) -> Option<u64> {
    let cleaned: String = digits.chars().filter(|c| *c != ',').collect();
    let base: f64 = cleaned.parse().ok()?;
    let scaled = if k_suffix.is_some() {
        base * 1000.0
    } else {
        base
    };
    // Sanity: yearly salaries are 5–7 figures; hourly rates are 2–4 figures.
    // Reject implausible magnitudes (years, counts, etc. that happen to
    // match the shape).
    let amount = scaled as u64;
    if period == "hour" {
        (10..=2_000).contains(&amount).then_some(amount)
    } else {
        (10_000..=9_999_999).contains(&amount).then_some(amount)
    }
}

/// Determine the period from the period keyword *nearest* the matched
/// amount. A posting that says "$150,000 - $200,000 annually. We also pay
/// hourly contractors" is annual — "annually" sits right next to the
/// range, "hourly" paragraphs away. Defaults to "year" when no keyword is
/// nearby (salary ranges without a stated period are annual).
fn detect_period_near(text: &str, start: usize, end: usize) -> String {
    let window_start = text.floor_char_boundary(start.saturating_sub(120));
    let window_end = text.ceil_char_boundary((end + 120).min(text.len()));
    let window = &text[window_start..window_end];
    let mid = (start + end) / 2;

    let nearest = |re: &LazyLock<Regex>| {
        re.find_iter(window)
            .map(|m| {
                let pos = window_start + m.start();
                pos.abs_diff(mid)
            })
            .min()
    };
    match (nearest(&HOURLY_RE), nearest(&YEARLY_RE)) {
        (Some(h), Some(y)) if h < y => "hour".into(),
        (Some(_), None) => "hour".into(),
        _ => "year".into(),
    }
}

/// Req id patterns in free text: `Req ID: JR2018233`, `Requisition #26-00061`.
pub fn extract_req_id(text: &str) -> Option<String> {
    REQ_ID_RE
        .captures(text)
        .map(|caps| caps[1].to_string())
        // Must contain at least one digit (rules out plain words).
        .filter(|candidate| candidate.chars().any(|c| c.is_ascii_digit()))
}

/// Conservative company fallback for the HTML/file drop-in paths, derived
/// from the page title: "Staff Engineer — Acme", "Engineer – Acme",
/// "Engineer at Acme". Only fires when no other source produced a company;
/// feeds the blacklist gate ("never match blacklisted companies" must hold
/// on every ingest path) and display. A wrong company is worse than none,
/// so only the last title segment after a strong separator is used.
pub fn company_from_title(title: &str) -> Option<String> {
    for sep in [" — ", " – ", " at "] {
        if let Some(pos) = title.rfind(sep) {
            let candidate = title[pos + sep.len()..].trim();
            let candidate = candidate.split([',', '.', '|']).next().unwrap_or("").trim();
            if candidate.len() >= 2 && candidate.chars().any(|c| c.is_alphabetic()) {
                return Some(candidate.to_string());
            }
        }
    }
    None
}

/// Prettify a board slug into a display name: `berkshire-energy` →
/// `Berkshire Energy`. APIs generally don't return the company display
/// name; the slug is what the URL gives us.
pub fn prettify_slug(slug: &str) -> String {
    slug.split(['-', '_'])
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut chars = w.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── extract_comp ─────────────────────────────────────────────

    #[test]
    fn comp_range_with_commas() {
        let comp = extract_comp("Salary: $220,000 - $290,000 per year.").unwrap();
        assert_eq!(comp.min, Some(220_000));
        assert_eq!(comp.max, Some(290_000));
        assert_eq!(comp.period, "year");
    }

    #[test]
    fn comp_range_k_suffix() {
        let comp = extract_comp("Compensation $180k–$240k DOE").unwrap();
        assert_eq!(comp.min, Some(180_000));
        assert_eq!(comp.max, Some(240_000));
    }

    #[test]
    fn comp_range_with_to() {
        let comp = extract_comp("USD $150,000 to $190,000 annually").unwrap();
        assert_eq!(comp.min, Some(150_000));
        assert_eq!(comp.max, Some(190_000));
    }

    #[test]
    fn comp_hourly_period() {
        // Hourly rates are annualized at the edge (USD/year internally).
        let comp = extract_comp("Rate: $85 - $110 per hour").unwrap();
        assert_eq!(comp.period, "hour");
        assert_eq!(comp.min, Some(85 * 2080));
        assert_eq!(comp.max, Some(110 * 2080));
    }

    #[test]
    fn comp_rejects_implausible_amounts() {
        assert!(extract_comp("over 5,000 customers and $4,999 raised").is_none());
        assert!(extract_comp("founded in 2019").is_none());
    }

    #[test]
    fn comp_annual_range_survives_unrelated_hourly_mention() {
        // `detect_period` scans the whole posting, so an unrelated "hourly"
        // mention elsewhere flips the period to "hour" and the annual range
        // is then rejected as implausible. The range's own context should
        // determine the period.
        let comp =
            extract_comp("Salary: $150,000 - $200,000 annually. We also pay hourly contractors.")
                .unwrap();
        assert_eq!(comp.min, Some(150_000));
        assert_eq!(comp.max, Some(200_000));
        assert_eq!(comp.period, "year");
    }

    #[test]
    fn comp_rejects_above_annual_ceiling() {
        assert!(extract_comp("Salary: $10,000,000 per year").is_none());
    }

    #[test]
    fn comp_rejects_below_hourly_floor() {
        assert!(extract_comp("Rate: $5 per hour").is_none());
    }

    #[test]
    fn comp_absent() {
        assert!(extract_comp("Great team, great mission.").is_none());
    }

    // ── extract_req_id ───────────────────────────────────────────

    #[test]
    fn req_id_patterns() {
        assert_eq!(
            extract_req_id("Req ID: JR2018233"),
            Some("JR2018233".into())
        );
        assert_eq!(
            extract_req_id("Requisition #26-00061"),
            Some("26-00061".into())
        );
        assert_eq!(
            extract_req_id("Job ID: 8642163002"),
            Some("8642163002".into())
        );
        assert_eq!(extract_req_id("no identifier here"), None);
    }

    #[test]
    fn req_id_requires_a_digit() {
        // Rules out plain words that happen to match the shape.
        assert_eq!(extract_req_id("Req ID: ABCDEF"), None);
    }

    // ── remote detection ─────────────────────────────────────────

    #[test]
    fn negated_onsite_signals_are_not_confident_negatives() {
        // "not hybrid, fully remote" must NOT classify as Some(false) —
        // that's the invisible false rejection the gate policy avoids.
        assert_eq!(detect_remote(None, "not hybrid, fully remote"), Some(true));
        assert_eq!(
            detect_remote(None, "no on-site requirement; work from anywhere"),
            None
        );
        assert_eq!(
            detect_remote(Some("Remote"), "not a hybrid role"),
            Some(true)
        );
        // Positive cases still classify.
        assert_eq!(detect_remote(None, "hybrid role"), Some(false));
    }

    #[test]
    fn remote_from_location() {
        assert_eq!(detect_remote(Some("Remote, US"), ""), Some(true));
        assert_eq!(detect_remote(Some("San Francisco, CA"), ""), None);
        // A physical location plus a body that says remote is still remote.
        assert_eq!(
            detect_remote(Some("San Francisco, CA"), "This role is remote-friendly."),
            Some(true)
        );
    }

    #[test]
    fn remote_from_body_when_no_location() {
        assert_eq!(
            detect_remote(None, "This is a fully remote role."),
            Some(true)
        );
        assert_eq!(detect_remote(None, "Join us in our NYC office."), None);
    }

    #[test]
    fn remote_negations_are_not_evidence() {
        assert_eq!(detect_remote(Some("NYC"), "this role is not remote"), None);
        assert_eq!(detect_remote(None, "this role is not remote"), None);
        assert_eq!(detect_remote(None, "non-remote position"), None);
        assert_eq!(
            detect_remote(Some("Denver"), "remote is not an option here"),
            None
        );
        assert_eq!(detect_remote(None, "fully remote team"), Some(true));
    }

    #[test]
    fn explicit_onsite_signals_win_over_remote_mentions() {
        // Confirmed design: hybrid/on-site/in-office are confident negatives
        // even when "remote" appears somewhere in the posting.
        assert_eq!(
            detect_remote(Some("Denver, CO"), "hybrid role"),
            Some(false)
        );
        assert_eq!(
            detect_remote(None, "This is a hybrid position with remote flexibility."),
            Some(false)
        );
        assert_eq!(
            detect_remote(Some("Hybrid - San Francisco"), ""),
            Some(false)
        );
        assert_eq!(detect_remote(None, "on-site in Austin"), Some(false));
        assert_eq!(detect_remote(None, "3 days in-office weekly"), Some(false));
        assert_eq!(
            detect_remote(None, "must be located in the Bay Area"),
            Some(false)
        );
    }

    #[test]
    fn company_from_title_patterns() {
        assert_eq!(
            company_from_title("Staff Engineer — Acme"),
            Some("Acme".into())
        );
        assert_eq!(
            company_from_title("Engineer at Acme, Remote"),
            Some("Acme".into())
        );
        assert_eq!(company_from_title("Staff Engineer"), None);
        assert_eq!(company_from_title(""), None);
    }

    // ── prettify_slug ────────────────────────────────────────────

    #[test]
    fn prettify_board_slugs() {
        assert_eq!(prettify_slug("berkshire-energy"), "Berkshire Energy");
        assert_eq!(prettify_slug("nvidia"), "Nvidia");
        assert_eq!(prettify_slug("scale_to_win"), "Scale To Win");
    }

    // ── html extraction ──────────────────────────────────────────

    #[test]
    fn html_to_text_strips_tags() {
        let text = html_to_text("<h1>Title</h1><p>Body <b>text</b>.</p>");
        assert!(text.contains("Title"));
        assert!(text.contains("Body"));
        assert!(text.contains("text"));
        assert!(!text.contains("<b>"));
    }

    #[test]
    fn extract_json_ld_job_posting() {
        let html = r#"<html><head><title>Shell</title></head><body>
            <script type="application/ld+json">
            {"@context":"https://schema.org/","@type":"JobPosting",
             "title":"Principal Backend Engineer, Hub (US East Coast)",
             "description":"<p>We are hiring a <strong>Principal Backend Engineer</strong>.</p>",
             "hiringOrganization":{"@type":"Organization","name":"Docker"},
             "jobLocation":{"@type":"Place","address":{"@type":"PostalAddress","addressCountry":"United States"}},
             "identifier":{"@type":"PropertyValue","name":"Docker","value":"9d5c3e18-eaef-4ffb-b4f1-795742fcba98"},
             "jobLocationType":"TELECOMMUTE"}
            </script>
            </body></html>"#;
        let job = extract_json_ld(html).unwrap();
        assert_eq!(job.title, "Principal Backend Engineer, Hub (US East Coast)");
        assert_eq!(job.company.as_deref(), Some("Docker"));
        assert_eq!(
            job.req_id.as_deref(),
            Some("9d5c3e18-eaef-4ffb-b4f1-795742fcba98")
        );
        assert_eq!(job.location.as_deref(), Some("United States"));
        assert_eq!(job.remote, Some(true));
        assert!(job.description_html.contains("Principal Backend Engineer"));
    }

    #[test]
    fn extract_json_ld_returns_none_without_posting() {
        let html = r#"<html><body><p>No JSON-LD here.</p></body></html>"#;
        assert!(extract_json_ld(html).is_none());
    }

    #[test]
    fn json_ld_on_site_location_type_is_confident_non_remote() {
        // The historically documented jobLocationType vocabulary is
        // ON_SITE / HYBRID / TELECOMMUTE; a posting emitting ON_SITE is
        // a confident non-remote and must not degrade to unknown (gates
        // reject only confident non-remotes).
        let html = r#"<html><body>
            <script type="application/ld+json">
            {"@context":"https://schema.org/","@type":"JobPosting",
             "title":"Warehouse Associate","description":"<p>On-site role.</p>",
             "jobLocationType":"ON_SITE"}
            </script>
            </body></html>"#;
        let job = extract_json_ld(html).unwrap();
        assert_eq!(job.remote, Some(false));
    }

    #[test]
    fn json_ld_search_continues_past_untitled_posting() {
        // A candidate without a title must not abort the search; the next
        // posting in the graph may be usable.
        let html = r#"<html><body>
            <script type="application/ld+json">
            {"@graph":[
                {"@type":"JobPosting"},
                {"@type":"JobPosting","title":"Real Job","description":"<p>Do things.</p>"}
            ]}
            </script>
            </body></html>"#;
        let job = extract_json_ld(html).unwrap();
        assert_eq!(job.title, "Real Job");
    }

    #[test]
    fn json_ld_graph_container_finds_posting() {
        // SEO plugins emit a `@graph` array; the JobPosting inside it is
        // just as structured as a top-level one.
        let html = r#"<html><body>
            <script type="application/ld+json">
            {"@graph":[
                {"@type":"Organization","name":"Acme"},
                {"@type":"JobPosting","title":"Staff Engineer","description":"<p>Build platforms.</p>"}
            ]}
            </script>
            </body></html>"#;
        let job = extract_json_ld(html).unwrap();
        assert_eq!(job.title, "Staff Engineer");
    }

    #[test]
    fn readability_extracts_main_content() {
        let html = r#"<!DOCTYPE html><html><head><title>Staff Engineer — Acme</title></head>
        <body><nav>Home | About | Jobs</nav>
        <article><h1>Staff Engineer, Platform</h1>
        <p>Acme is hiring a Staff Engineer for our platform team. This role is
        remote within the US. Salary: $200,000 - $250,000. You will own our
        messaging infrastructure and mentor senior engineers across the org.
        We value written communication, operational excellence, and pragmatic
        systems thinking in everything we build and operate at scale daily.</p>
        </article><footer>Copyright 2026 Acme</footer></body></html>"#;
        let url = Url::parse("https://example.com/careers/staff-engineer").unwrap();
        let (title, text) = extract_main_text(html, &url).unwrap();
        assert!(title.is_some());
        assert!(text.contains("Staff Engineer"));
        assert!(text.contains("$200,000 - $250,000"));
    }
}
