//! Lead identity: dedupe key computation and canonicalization.
//!
//! Design doc 0001 §2. Precedence: `req:<company-slug>:<req-id>` →
//! `url:<canonical-url>` → `tc:<sha256(title+company)>`. All computable forms
//! are stored and indexed; matching consults `tc:` only when the incoming
//! posting carries neither req nor URL.

use sha2::{Digest, Sha256};
use url::Url;

use crate::domain::events::{ExtractedFields, Identifiers};

/// Query parameters always dropped during URL canonicalization — known
/// tracking params. Everything else is preserved, because some boards put
/// the job id in the query (e.g. `?jobId=123`).
const TRACKING_PARAMS: &[&str] = &[
    "fbclid", "gclid", "gclsrc", "dclid", "msclkid", "mc_cid", "mc_eid", "igshid", "_ga", "_gl",
    "ref_src", "spm", "wickedid", "vero_id",
];

/// A lead's computed identity: the dedupe key (strongest applicable form)
/// plus every identifier form we could compute.
#[derive(Clone, Debug)]
pub struct LeadIdentity {
    pub dedupe_key: String,
    pub identifiers: Identifiers,
}

pub fn compute_identity(
    extracted: &ExtractedFields,
    url: Option<&Url>,
    raw_text: &str,
) -> LeadIdentity {
    let req = match (&extracted.company, &extracted.req_id) {
        (Some(company), Some(req_id)) => Some(format!(
            "req:{}:{}",
            slugify(company),
            normalize_req(req_id)
        )),
        _ => None,
    };

    let url_key = url.map(|u| format!("url:{}", canonicalize_url(u)));

    let tc = match (&extracted.title, &extracted.company) {
        (Some(title), Some(company)) => Some(format!(
            "tc:{}",
            sha256_hex(&format!(
                "{}\n{}",
                normalize_text(title),
                normalize_text(company)
            ))
        )),
        _ => None,
    };

    // Precedence: req → url → tc → raw-text hash (last resort for postings
    // with no usable structured fields at all).
    let dedupe_key = req
        .clone()
        .or_else(|| url_key.clone())
        .or_else(|| tc.clone())
        .unwrap_or_else(|| format!("raw:{}", sha256_hex(raw_text)));

    LeadIdentity {
        dedupe_key,
        identifiers: Identifiers {
            req,
            url: url_key,
            tc,
        },
    }
}

/// Canonicalize a posting URL: fragment dropped, known tracking params
/// dropped (unknown params preserved and sorted), trailing slashes
/// collapsed. Scheme/host case and default ports are already normalized by
/// the `url` crate.
pub fn canonicalize_url(url: &Url) -> String {
    let mut canonical = url.clone();
    canonical.set_fragment(None);

    let mut params: Vec<(String, String)> = canonical
        .query_pairs()
        .filter(|(name, _)| !name.starts_with("utm_") && !TRACKING_PARAMS.contains(&name.as_ref()))
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    params.sort();
    if params.is_empty() {
        canonical.set_query(None);
    } else {
        let query: String = params
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("&");
        canonical.set_query(Some(&query));
    }

    let path = canonical.path().trim_end_matches('/').to_string();
    canonical.set_path(if path.is_empty() { "/" } else { &path });

    canonical.to_string()
}

/// Lowercase, whitespace-collapsed, non-alphanumerics folded to `-`.
pub fn slugify(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_dash = true; // trim leading
    for c in s.trim().chars().flat_map(char::to_lowercase) {
        if c.is_alphanumeric() {
            out.push(c);
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    out.trim_end_matches('-').to_string()
}

fn normalize_req(req_id: &str) -> String {
    req_id
        .split_whitespace()
        .collect::<Vec<_>>()
        .join("")
        .to_lowercase()
}

fn normalize_text(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

pub fn sha256_hex(input: &str) -> String {
    let digest = Sha256::digest(input.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extracted(
        title: Option<&str>,
        company: Option<&str>,
        req_id: Option<&str>,
    ) -> ExtractedFields {
        ExtractedFields {
            title: title.map(Into::into),
            company: company.map(Into::into),
            req_id: req_id.map(Into::into),
            ..Default::default()
        }
    }

    // ── canonicalize_url ─────────────────────────────────────────

    #[test]
    fn drops_tracking_params_keeps_job_id() {
        let url =
            Url::parse("https://example.com/job?id=123&utm_source=li&fbclid=xyz&r=A%2FB").unwrap();
        assert_eq!(
            canonicalize_url(&url),
            "https://example.com/job?id=123&r=A/B"
        );
    }

    #[test]
    fn drops_fragment_and_trailing_slash() {
        let url = Url::parse("https://example.com/job/123/#apply").unwrap();
        assert_eq!(canonicalize_url(&url), "https://example.com/job/123");
    }

    #[test]
    fn sorts_remaining_params() {
        let url = Url::parse("https://example.com/j?b=2&a=1").unwrap();
        assert_eq!(canonicalize_url(&url), "https://example.com/j?a=1&b=2");
    }

    #[test]
    fn drops_default_port_and_lowercases_host() {
        let url = Url::parse("https://EXAMPLE.com:443/Job").unwrap();
        assert_eq!(canonicalize_url(&url), "https://example.com/Job");
    }

    #[test]
    fn keeps_non_default_port() {
        let url = Url::parse("https://example.com:8443/job").unwrap();
        assert_eq!(canonicalize_url(&url), "https://example.com:8443/job");
    }

    // ── slugify / normalize ──────────────────────────────────────

    #[test]
    fn slugify_basic() {
        assert_eq!(slugify("Acme Corp"), "acme-corp");
        assert_eq!(slugify("  Scale   to   Win "), "scale-to-win");
        assert_eq!(slugify("Berkshire-Energy!"), "berkshire-energy");
        assert_eq!(slugify("D.E. Shaw & Co."), "d-e-shaw-co");
    }

    // ── compute_identity precedence ──────────────────────────────

    #[test]
    fn req_form_wins_when_company_and_req_present() {
        let id = compute_identity(
            &extracted(Some("Engineer"), Some("NVIDIA"), Some("JR2018233")),
            Some(&Url::parse("https://example.com/j").unwrap()),
            "body",
        );
        assert_eq!(id.dedupe_key, "req:nvidia:jr2018233");
        assert_eq!(id.identifiers.req.as_deref(), Some("req:nvidia:jr2018233"));
        assert!(id.identifiers.url.is_some());
        assert!(id.identifiers.tc.is_some());
    }

    #[test]
    fn url_form_when_no_req() {
        let id = compute_identity(
            &extracted(Some("Engineer"), Some("Acme"), None),
            Some(&Url::parse("https://example.com/j/?utm_source=x").unwrap()),
            "body",
        );
        assert_eq!(id.dedupe_key, "url:https://example.com/j");
        assert!(id.identifiers.req.is_none());
        assert!(id.identifiers.tc.is_some());
    }

    #[test]
    fn tc_form_when_no_url_or_req() {
        let id = compute_identity(
            &extracted(Some("Staff Engineer"), Some("Acme"), None),
            None,
            "body",
        );
        assert!(id.dedupe_key.starts_with("tc:"));
        assert!(id.identifiers.url.is_none());
    }

    #[test]
    fn tc_is_case_and_whitespace_insensitive() {
        let a = compute_identity(
            &extracted(Some("Staff   Engineer"), Some("ACME"), None),
            None,
            "body",
        );
        let b = compute_identity(
            &extracted(Some("staff engineer"), Some("acme"), None),
            None,
            "other body",
        );
        assert_eq!(a.dedupe_key, b.dedupe_key);
    }

    #[test]
    fn raw_fallback_when_nothing_structured() {
        let id = compute_identity(&ExtractedFields::default(), None, "some body");
        assert!(id.dedupe_key.starts_with("raw:"));
        let same = compute_identity(&ExtractedFields::default(), None, "some body");
        assert_eq!(id.dedupe_key, same.dedupe_key);
        let different = compute_identity(&ExtractedFields::default(), None, "other body");
        assert_ne!(id.dedupe_key, different.dedupe_key);
    }
}
