//! Platform detection and public JSON API adapters.
//!
//! The drop-in adapter is platform-aware (design discussion on GWLJ-g8gbo3):
//! a dropped URL that matches a known board is fetched via the board's
//! public JSON API first — structured, ToS-friendly, and robust against
//! anti-scraping frontends — with HTML fetch + main-text extraction as the
//! fallback for unknown sites. Discovery/polling watchlists are vNext.
//!
//! All response parsing is defensive: these are undocumented public APIs
//! and their shapes drift, so every field read goes through `Value` probing
//! rather than a rigid struct.

use miette::{Context, Result, bail};
use serde_json::Value;
use url::Url;

use crate::domain::events::ExtractedFields;
use crate::ingest::extract;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Platform {
    Greenhouse,
    Ashby,
    Lever,
    Workday,
}

impl Platform {
    pub fn source_name(&self) -> &'static str {
        match self {
            Platform::Greenhouse => "greenhouse",
            Platform::Ashby => "ashby",
            Platform::Lever => "lever",
            Platform::Workday => "workday",
        }
    }
}

/// What an API gave us, pre-field-extraction.
#[derive(Clone, Debug)]
pub struct ApiExtraction {
    pub source: &'static str,
    pub title: Option<String>,
    pub company: Option<String>,
    pub req_id: Option<String>,
    pub location: Option<String>,
    pub body_html: String,
}

/// Detect a known board from the posting URL's host.
pub fn detect(url: &Url) -> Option<Platform> {
    let host = url.host_str()?.to_lowercase();
    if host == "job-boards.greenhouse.io" || host == "boards.greenhouse.io" {
        Some(Platform::Greenhouse)
    } else if host == "jobs.ashbyhq.com" {
        Some(Platform::Ashby)
    } else if host == "jobs.lever.co" {
        Some(Platform::Lever)
    } else if host.ends_with(".myworkdayjobs.com") {
        Some(Platform::Workday)
    } else {
        None
    }
}

/// Build the public JSON API URL for a posting URL, or `None` if the URL's
/// shape isn't recognized (caller falls back to HTML fetch).
pub fn api_url(url: &Url, platform: Platform) -> Option<Url> {
    let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    let built = match (platform, segments.as_slice()) {
        // /{board}/jobs/{id}
        (Platform::Greenhouse, [board, "jobs", id, ..]) => Some(format!(
            "https://boards-api.greenhouse.io/v1/boards/{board}/jobs/{id}"
        )),
        // /{board}/{jobId}
        (Platform::Ashby, [board, job_id, ..]) => Some(format!(
            "https://api.ashbyhq.com/posting-api/job-board/{board}/{job_id}"
        )),
        // /{company}/{id}
        (Platform::Lever, [company, id, ..]) => {
            Some(format!("https://api.lever.co/v0/postings/{company}/{id}"))
        }
        (Platform::Workday, _) => workday_api_url(url, &segments),
        _ => None,
    }?;
    Url::parse(&built).ok()
}

/// Workday posting URLs: `https://{tenant}.wdN.myworkdayjobs.com[/{locale}]/{site}/job/{path...}`
/// CXS JSON: `https://{host}/wday/cxs/{tenant}/{site}/job/{path...}`.
fn workday_api_url(url: &Url, segments: &[&str]) -> Option<String> {
    let host = url.host_str()?;
    let tenant = host.split('.').next()?;

    // Drop a locale prefix like `en-US` if present.
    let locale_re = regex::Regex::new(r"^[a-z]{2}(-[A-Z]{2})?$").expect("locale regex compiles");
    let segments: &[&str] = match segments {
        [first, rest @ ..] if locale_re.is_match(first) && !rest.is_empty() => rest,
        other => other,
    };
    let [site, "job", job_path @ ..] = segments else {
        return None;
    };
    if job_path.is_empty() {
        return None;
    }
    Some(format!(
        "https://{host}/wday/cxs/{tenant}/{site}/job/{}",
        job_path.join("/")
    ))
}

/// Parse an API response body into an extraction. `board_slug` is used as a
/// company-name fallback (prettified) when the API doesn't return a display
/// name.
pub fn parse_api_response(
    platform: Platform,
    body: &Value,
    posting_url: &Url,
) -> Result<ApiExtraction> {
    match platform {
        Platform::Greenhouse => parse_greenhouse(body, posting_url),
        Platform::Ashby => parse_ashby(body, posting_url),
        Platform::Lever => parse_lever(body, posting_url),
        Platform::Workday => parse_workday(body, posting_url),
    }
}

fn board_slug(url: &Url) -> Option<String> {
    url.path_segments()?
        .find(|s| !s.is_empty())
        .map(str::to_string)
}

fn str_field<'a>(value: &'a Value, path: &[&str]) -> Option<&'a str> {
    let mut current = value;
    for key in path {
        current = current.get(*key)?;
    }
    current.as_str()
}

fn id_field(value: &Value, path: &[&str]) -> Option<String> {
    let mut current = value;
    for key in path {
        current = current.get(*key)?;
    }
    current
        .as_str()
        .map(str::to_string)
        .or_else(|| current.as_u64().map(|n| n.to_string()))
}

fn parse_greenhouse(body: &Value, posting_url: &Url) -> Result<ApiExtraction> {
    let title = str_field(body, &["title"]).map(str::to_string);
    if title.is_none() {
        bail!("greenhouse API response missing title (shape changed?)");
    }
    Ok(ApiExtraction {
        source: Platform::Greenhouse.source_name(),
        title,
        company: board_slug(posting_url).map(|s| extract::prettify_slug(&s)),
        req_id: id_field(body, &["id"]),
        location: str_field(body, &["location", "name"]).map(str::to_string),
        body_html: str_field(body, &["content"])
            .unwrap_or_default()
            .to_string(),
    })
}

fn parse_ashby(body: &Value, posting_url: &Url) -> Result<ApiExtraction> {
    let title = str_field(body, &["title"]).map(str::to_string);
    if title.is_none() {
        bail!("ashby API response missing title (shape changed?)");
    }
    let location = str_field(body, &["locationName"])
        .or_else(|| str_field(body, &["location"]))
        .map(str::to_string);
    Ok(ApiExtraction {
        source: Platform::Ashby.source_name(),
        title,
        company: board_slug(posting_url).map(|s| extract::prettify_slug(&s)),
        req_id: id_field(body, &["id"]),
        location,
        body_html: str_field(body, &["descriptionHtml"])
            .unwrap_or_default()
            .to_string(),
    })
}

fn parse_lever(body: &Value, posting_url: &Url) -> Result<ApiExtraction> {
    let title = str_field(body, &["text"]).map(str::to_string);
    if title.is_none() {
        bail!("lever API response missing text (shape changed?)");
    }
    // Lever returns `descriptionPlain` plus optional `lists` and
    // `additional` (HTML). Prefer plain text; fall back to the HTML parts.
    let body_html = if let Some(plain) = str_field(body, &["descriptionPlain"]) {
        let mut text = plain.to_string();
        if let Some(lists) = body.get("lists").and_then(Value::as_array) {
            for list in lists {
                if let Some(content) = str_field(list, &["content"]) {
                    text.push('\n');
                    text.push_str(&extract::html_to_text(content));
                }
            }
        }
        text
    } else {
        let mut html = str_field(body, &["description"])
            .unwrap_or_default()
            .to_string();
        if let Some(lists) = body.get("lists").and_then(Value::as_array) {
            for list in lists {
                if let Some(content) = str_field(list, &["content"]) {
                    html.push_str(content);
                }
            }
        }
        html
    };
    Ok(ApiExtraction {
        source: Platform::Lever.source_name(),
        title,
        company: board_slug(posting_url).map(|s| extract::prettify_slug(&s)),
        req_id: id_field(body, &["id"]),
        location: str_field(body, &["categories", "location"]).map(str::to_string),
        body_html,
    })
}

fn parse_workday(body: &Value, posting_url: &Url) -> Result<ApiExtraction> {
    let info = body
        .get("jobPostingInfo")
        .wrap_err("workday CXS response missing jobPostingInfo (shape changed?)")?;
    // Workday CXS doesn't return the company display name; the tenant
    // subdomain is the company's identity on the platform.
    let company = str_field(info, &["company"])
        .map(str::to_string)
        .or_else(|| str_field(info, &["companyName"]).map(str::to_string))
        .or_else(|| {
            posting_url
                .host_str()
                .and_then(|h| h.split('.').next())
                .map(extract::prettify_slug)
        });
    Ok(ApiExtraction {
        source: Platform::Workday.source_name(),
        title: str_field(info, &["title"]).map(str::to_string),
        company,
        req_id: str_field(info, &["jobReqId"]).map(str::to_string),
        location: str_field(info, &["location"]).map(str::to_string),
        body_html: str_field(info, &["jobDescription"])
            .unwrap_or_default()
            .to_string(),
    })
}

/// Merge an API extraction with text-derived fields into the final
/// `ExtractedFields`. Text extraction (comp, remote, req-id fallback) runs
/// over the body regardless — API comp coverage is spotty.
pub fn finalize(api: &ApiExtraction) -> (String, ExtractedFields) {
    let raw_text = if api.body_html.contains('<') {
        extract::html_to_text(&api.body_html)
    } else {
        api.body_html.clone()
    };
    let mut fields = extract::extract_fields(&raw_text, api.location.as_deref());
    fields.title = api.title.clone();
    fields.company = api.company.clone();
    fields.req_id = api
        .req_id
        .clone()
        .or_else(|| extract::extract_req_id(&raw_text));
    (raw_text, fields)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── detect ───────────────────────────────────────────────────

    #[test]
    fn detects_known_boards() {
        let cases = [
            (
                "https://job-boards.greenhouse.io/acme/jobs/123",
                Platform::Greenhouse,
            ),
            (
                "https://boards.greenhouse.io/acme/jobs/123",
                Platform::Greenhouse,
            ),
            ("https://jobs.ashbyhq.com/acme/abc-123", Platform::Ashby),
            ("https://jobs.lever.co/acme/abc-123", Platform::Lever),
            (
                "https://nvidia.wd5.myworkdayjobs.com/en-US/NVIDIAExternalCareerSite/job/X_JR2018233",
                Platform::Workday,
            ),
        ];
        for (url, platform) in cases {
            assert_eq!(detect(&Url::parse(url).unwrap()), Some(platform), "{url}");
        }
        assert_eq!(
            detect(&Url::parse("https://example.com/careers/eng").unwrap()),
            None
        );
    }

    // ── api_url ──────────────────────────────────────────────────

    #[test]
    fn greenhouse_api_url() {
        let url = Url::parse("https://job-boards.greenhouse.io/later/jobs/8642163002").unwrap();
        assert_eq!(
            api_url(&url, Platform::Greenhouse).unwrap().as_str(),
            "https://boards-api.greenhouse.io/v1/boards/later/jobs/8642163002"
        );
    }

    #[test]
    fn ashby_api_url() {
        let url =
            Url::parse("https://jobs.ashbyhq.com/restate/fe90faab-5417-4034-b915-0770e2477a73")
                .unwrap();
        assert_eq!(
            api_url(&url, Platform::Ashby).unwrap().as_str(),
            "https://api.ashbyhq.com/posting-api/job-board/restate/fe90faab-5417-4034-b915-0770e2477a73"
        );
    }

    #[test]
    fn lever_api_url() {
        let url = Url::parse("https://jobs.lever.co/acme/abc-123").unwrap();
        assert_eq!(
            api_url(&url, Platform::Lever).unwrap().as_str(),
            "https://api.lever.co/v0/postings/acme/abc-123"
        );
    }

    #[test]
    fn workday_api_url_strips_locale() {
        let url = Url::parse(
            "https://nvidia.wd5.myworkdayjobs.com/en-US/NVIDIAExternalCareerSite/job/Principal-Software-Engineer--DGX-Cloud-Production-Engineering_JR2018233",
        )
        .unwrap();
        assert_eq!(
            api_url(&url, Platform::Workday).unwrap().as_str(),
            "https://nvidia.wd5.myworkdayjobs.com/wday/cxs/nvidia/NVIDIAExternalCareerSite/job/Principal-Software-Engineer--DGX-Cloud-Production-Engineering_JR2018233"
        );
    }

    #[test]
    fn workday_api_url_without_locale() {
        let url =
            Url::parse("https://acme.wd3.myworkdayjobs.com/AcmeCareers/job/Senior-Engineer_R-123")
                .unwrap();
        assert_eq!(
            api_url(&url, Platform::Workday).unwrap().as_str(),
            "https://acme.wd3.myworkdayjobs.com/wday/cxs/acme/AcmeCareers/job/Senior-Engineer_R-123"
        );
    }

    #[test]
    fn unrecognized_shapes_return_none() {
        let url = Url::parse("https://jobs.lever.co/").unwrap();
        assert_eq!(api_url(&url, Platform::Lever), None);
        let url = Url::parse("https://nvidia.wd5.myworkdayjobs.com/en-US").unwrap();
        assert_eq!(api_url(&url, Platform::Workday), None);
    }

    // ── parse_api_response ───────────────────────────────────────

    #[test]
    fn parses_greenhouse_response() {
        let body = serde_json::json!({
            "id": 8642163002i64,
            "title": "Staff Engineer (Platform)",
            "location": {"name": "Remote"},
            "content": "<p>Salary: $180,000 - $220,000. Build things.</p>"
        });
        let url = Url::parse("https://job-boards.greenhouse.io/later/jobs/8642163002").unwrap();
        let api = parse_api_response(Platform::Greenhouse, &body, &url).unwrap();
        assert_eq!(api.title.as_deref(), Some("Staff Engineer (Platform)"));
        assert_eq!(api.company.as_deref(), Some("Later"));
        assert_eq!(api.req_id.as_deref(), Some("8642163002"));
        assert_eq!(api.location.as_deref(), Some("Remote"));

        let (raw_text, fields) = finalize(&api);
        assert!(raw_text.contains("Salary:"));
        assert_eq!(fields.comp.unwrap().min, Some(180_000));
        assert_eq!(fields.remote, Some(true));
    }

    #[test]
    fn parses_ashby_response() {
        let body = serde_json::json!({
            "id": "fe90faab-5417-4034-b915-0770e2477a73",
            "title": "Senior Cloud Infrastructure Engineer",
            "locationName": "Remote (US)",
            "descriptionHtml": "<p>Join us.</p>"
        });
        let url = Url::parse("https://jobs.ashbyhq.com/restate/fe90faab").unwrap();
        let api = parse_api_response(Platform::Ashby, &body, &url).unwrap();
        assert_eq!(api.company.as_deref(), Some("Restate"));
        assert_eq!(api.location.as_deref(), Some("Remote (US)"));
    }

    #[test]
    fn parses_lever_response_with_lists() {
        let body = serde_json::json!({
            "id": "abc-123",
            "text": "Staff Software Engineer",
            "categories": {"location": "Remote"},
            "descriptionPlain": "We are hiring.",
            "lists": [{"text": "Requirements", "content": "<ul><li>5+ years</li></ul>"}]
        });
        let url = Url::parse("https://jobs.lever.co/acme/abc-123").unwrap();
        let api = parse_api_response(Platform::Lever, &body, &url).unwrap();
        let (raw_text, _) = finalize(&api);
        assert!(raw_text.contains("We are hiring."));
        assert!(raw_text.contains("5+ years"));
    }

    #[test]
    fn parses_workday_response() {
        let body = serde_json::json!({
            "jobPostingInfo": {
                "title": "Principal Software Engineer: DGX Cloud",
                "jobReqId": "JR2018233",
                "location": "US, Remote",
                "jobDescription": "<p>Compensation: $220,000 - $290,000.</p>"
            }
        });
        let url =
            Url::parse("https://nvidia.wd5.myworkdayjobs.com/en-US/Site/job/X_JR2018233").unwrap();
        let api = parse_api_response(Platform::Workday, &body, &url).unwrap();
        assert_eq!(api.req_id.as_deref(), Some("JR2018233"));
        let (_, fields) = finalize(&api);
        assert_eq!(fields.comp.unwrap().max, Some(290_000));
        assert_eq!(fields.remote, Some(true));
        assert_eq!(api.company.as_deref(), Some("Nvidia"));
    }

    #[test]
    fn missing_title_is_an_error() {
        let body = serde_json::json!({"error": "Job not found"});
        let url = Url::parse("https://job-boards.greenhouse.io/acme/jobs/1").unwrap();
        assert!(parse_api_response(Platform::Greenhouse, &body, &url).is_err());
    }
}
