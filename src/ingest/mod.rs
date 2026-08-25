//! Source adapters: fetch and extraction (design doc 0001 §9; v0 ships the
//! platform-aware drop-in adapter only — see `platforms`).

use std::path::Path;
use std::time::Duration;

use miette::{Context, IntoDiagnostic, Result, bail, miette};
use tracing::{debug, instrument, warn};
use url::Url;

use crate::domain::events::ExtractedFields;

pub mod extract;
pub mod platforms;

/// Politeness delay before every HTTP request (spec: 200–500ms).
const POLITENESS_DELAY: Duration = Duration::from_millis(300);
const MAX_RETRIES: u32 = 1;

/// The result of fetching and extracting a posting, ready for identity
/// computation and the `ingested`/`updated` payload.
#[derive(Clone, Debug)]
pub struct IngestOutcome {
    pub source: String,
    pub url: Option<String>,
    pub raw_text: String,
    pub extracted: ExtractedFields,
}

/// HTTP client with good-citizen behavior baked in: politeness delay,
/// honest user agent, redirect following (reqwest default), and one retry
/// honoring `Retry-After` on 429/503.
pub struct HttpClient {
    client: reqwest::Client,
}

impl HttpClient {
    pub fn new() -> Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent(concat!(
                env!("CARGO_PKG_NAME"),
                "/",
                env!("CARGO_PKG_VERSION"),
                " (+https://github.com/plathrop/gwl-job-tools)"
            ))
            .build()
            .into_diagnostic()?;
        Ok(Self { client })
    }

    async fn get(&self, url: &Url) -> Result<reqwest::Response> {
        let mut attempt = 0;
        loop {
            // Be a good citizen: fixed delay between requests.
            tokio::time::sleep(POLITENESS_DELAY).await;
            debug!(%url, attempt, "fetching");
            let response = self
                .client
                .get(url.clone())
                .send()
                .await
                .into_diagnostic()?;
            let status = response.status();
            if (status == reqwest::StatusCode::TOO_MANY_REQUESTS
                || status == reqwest::StatusCode::SERVICE_UNAVAILABLE)
                && attempt < MAX_RETRIES
            {
                // Obey control headers.
                let retry_after = response
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(2)
                    .min(10);
                warn!(%url, retry_after, "rate-limited; honoring Retry-After");
                tokio::time::sleep(Duration::from_secs(retry_after)).await;
                attempt += 1;
                continue;
            }
            if !status.is_success() {
                bail!("fetching {url} failed with status {status}");
            }
            return Ok(response);
        }
    }

    pub async fn get_text(&self, url: &Url) -> Result<String> {
        self.get(url).await?.text().await.into_diagnostic()
    }

    pub async fn get_json(&self, url: &Url) -> Result<serde_json::Value> {
        self.get(url).await?.json().await.into_diagnostic()
    }
}

/// Ingest a posting URL: public JSON API first for known boards, HTML fetch
/// + main-text extraction as the fallback (unknown sites, API failures with
/// unrecognized response shapes).
#[instrument(skip(http), fields(url = %url))]
pub async fn ingest_url(url: &Url, http: &HttpClient) -> Result<IngestOutcome> {
    if let Some(platform) = platforms::detect(url) {
        match ingest_via_api(url, platform, http).await {
            Ok(outcome) => return Ok(outcome),
            Err(err) => {
                warn!(
                    error = %err,
                    platform = platform.source_name(),
                    "API extraction failed; falling back to HTML fetch"
                );
            }
        }
    }
    ingest_via_html(url, http).await
}

async fn ingest_via_api(
    url: &Url,
    platform: platforms::Platform,
    http: &HttpClient,
) -> Result<IngestOutcome> {
    let api_url = platforms::api_url(url, platform)
        .ok_or_else(|| miette!("could not build API URL for {url}"))?;
    let body = http
        .get_json(&api_url)
        .await
        .wrap_err_with(|| format!("fetching {} API at {api_url}", platform.source_name()))?;
    let extraction = platforms::parse_api_response(platform, &body, url)?;
    let (raw_text, extracted) = platforms::finalize(&extraction);
    if raw_text.trim().is_empty() {
        bail!(
            "{} API returned an empty body for {url}",
            platform.source_name()
        );
    }
    Ok(IngestOutcome {
        source: platform.source_name().into(),
        url: Some(url.to_string()),
        raw_text,
        extracted,
    })
}

async fn ingest_via_html(url: &Url, http: &HttpClient) -> Result<IngestOutcome> {
    let html = http
        .get_text(url)
        .await
        .wrap_err_with(|| format!("fetching {url}"))?;
    let (title, raw_text) = extract::extract_main_text(&html, url)?;
    if raw_text.trim().is_empty() {
        bail!("no text could be extracted from {url}");
    }
    let mut extracted = extract::extract_fields(&raw_text, None);
    extracted.title = title;
    extracted.req_id = extract::extract_req_id(&raw_text);
    Ok(IngestOutcome {
        source: "drop-in".into(),
        url: Some(url.to_string()),
        raw_text,
        extracted,
    })
}

/// Ingest a local file drop: `.html`/`.htm` goes through main-text
/// extraction; anything else is treated as plain text.
#[instrument(skip_all, fields(path = %path.display()))]
pub fn ingest_file(path: &Path, content: &str) -> Result<IngestOutcome> {
    let is_html = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("html") || e.eq_ignore_ascii_case("htm"));

    let (title, raw_text) = if is_html {
        // No base URL for a file drop; a synthetic one keeps the extractor
        // happy and never appears in identifiers (no `url:` form is minted).
        let base = Url::parse("https://localhost/").expect("valid base URL");
        extract::extract_main_text(content, &base)?
    } else {
        (None, content.to_string())
    };
    if raw_text.trim().is_empty() {
        bail!("no text could be extracted from {}", path.display());
    }

    let mut extracted = extract::extract_fields(&raw_text, None);
    extracted.title = title;
    extracted.req_id = extract::extract_req_id(&raw_text);
    Ok(IngestOutcome {
        source: "drop-in".into(),
        url: None,
        raw_text,
        extracted,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn file_drop_plain_text() {
        let outcome = ingest_file(
            &PathBuf::from("jd.txt"),
            "Staff Engineer at Acme. Remote. Salary: $200,000 - $250,000. Req ID: R-12345.",
        )
        .unwrap();
        assert_eq!(outcome.source, "drop-in");
        assert_eq!(outcome.url, None);
        assert_eq!(outcome.extracted.req_id.as_deref(), Some("R-12345"));
        assert_eq!(outcome.extracted.comp.unwrap().min, Some(200_000));
        assert_eq!(outcome.extracted.remote, Some(true));
    }

    #[test]
    fn file_drop_html_extracts_main_text() {
        let html = "<html><head><title>Engineer — Acme</title></head><body>\
                    <nav>menu</nav><article><h1>Platform Engineer</h1>\
                    <p>Remote role building platforms with a wonderful team of \
                    experienced engineers who care deeply about reliability, \
                    operability, and mentoring across the organization every day.</p>\
                    </article></body></html>";
        let outcome = ingest_file(&PathBuf::from("jd.html"), html).unwrap();
        assert!(outcome.raw_text.contains("Platform Engineer"));
        assert_eq!(outcome.extracted.title.as_deref(), Some("Engineer — Acme"));
    }

    #[test]
    fn file_drop_empty_bails() {
        assert!(ingest_file(&PathBuf::from("jd.txt"), "   ").is_err());
    }
}
