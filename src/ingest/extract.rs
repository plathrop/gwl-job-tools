//! Best-effort, deterministic field extraction from posting text and HTML.
//!
//! Comp coverage is spotty everywhere (APIs and sites alike); these
//! heuristics take what exists and leave the rest absent.

use dom_smoothie::Readability;
use miette::Result;
use regex::Regex;
use url::Url;

use crate::domain::events::{CompRange, ExtractedFields};

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

fn detect_remote(location: Option<&str>, text: &str) -> Option<bool> {
    if let Some(loc) = location {
        return Some(loc.to_lowercase().contains("remote"));
    }
    // No location at all: look for an explicit "remote" mention in the
    // first chunk of the body (best-effort).
    let head: String = text.chars().take(2000).collect();
    if Regex::new(r"(?i)\bremote\b").unwrap().is_match(&head) {
        Some(true)
    } else {
        None
    }
}

/// Salary range patterns: `$220,000 - $290,000`, `$220k–$290k`,
/// `USD 220,000 to 290,000`. Single amounts (`$180,000/yr`) set `min` only.
pub fn extract_comp(text: &str) -> Option<CompRange> {
    let period = detect_period(text);
    let range_re = Regex::new(
        r"(?i)(?:USD\s*)?\$\s*(\d{2,3}(?:,\d{3})+|\d+(?:\.\d+)?)\s*(k)?\s*(?:-|–|—|\bto\b)\s*(?:USD\s*)?\$?\s*(\d{2,3}(?:,\d{3})+|\d+(?:\.\d+)?)\s*(k)?",
    )
    .unwrap();
    if let Some(caps) = range_re.captures(text) {
        let min = parse_amount(&caps[1], caps.get(2).map(|m| m.as_str()), &period);
        let max = parse_amount(&caps[3], caps.get(4).map(|m| m.as_str()), &period);
        if let (Some(min), Some(max)) = (min, max) {
            let raw = caps[0].trim().to_string();
            return Some(CompRange {
                min: Some(min),
                max: Some(max),
                currency: "USD".into(),
                period,
                raw,
            });
        }
    }

    let single_re = Regex::new(r"(?i)(?:USD\s*)?\$\s*(\d{2,3}(?:,\d{3})+)\s*(k)?").unwrap();
    if let Some(caps) = single_re.captures(text) {
        if let Some(amount) = parse_amount(&caps[1], caps.get(2).map(|m| m.as_str()), &period) {
            let raw = caps[0].trim().to_string();
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

fn detect_period(text: &str) -> String {
    let re = Regex::new(r"(?i)\b(per\s+hour|hourly|/hr|an\s+hour)\b").unwrap();
    if re.is_match(text) {
        "hour".into()
    } else {
        "year".into()
    }
}

/// Req id patterns in free text: `Req ID: JR2018233`, `Requisition #26-00061`.
pub fn extract_req_id(text: &str) -> Option<String> {
    let re = Regex::new(
        r"(?i)\b(?:req(?:uisition)?(?:\s*id)?|job\s*(?:id|number|requisition)|requisition\s*id)\s*[:#]?\s*([A-Z0-9][A-Z0-9-]{2,})\b",
    )
    .unwrap();
    re.captures(text)
        .map(|caps| caps[1].to_string())
        // Must contain at least one digit (rules out plain words).
        .filter(|candidate| candidate.chars().any(|c| c.is_ascii_digit()))
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
        let comp = extract_comp("Rate: $85 - $110 per hour").unwrap();
        assert_eq!(comp.period, "hour");
        assert_eq!(comp.min, Some(85));
        assert_eq!(comp.max, Some(110));
    }

    #[test]
    fn comp_rejects_implausible_amounts() {
        assert!(extract_comp("over 5,000 customers and $4,999 raised").is_none());
        assert!(extract_comp("founded in 2019").is_none());
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

    // ── remote detection ─────────────────────────────────────────

    #[test]
    fn remote_from_location() {
        assert_eq!(detect_remote(Some("Remote, US"), ""), Some(true));
        assert_eq!(detect_remote(Some("San Francisco, CA"), ""), Some(false));
    }

    #[test]
    fn remote_from_body_when_no_location() {
        assert_eq!(
            detect_remote(None, "This is a fully remote role."),
            Some(true)
        );
        assert_eq!(detect_remote(None, "Join us in our NYC office."), None);
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
