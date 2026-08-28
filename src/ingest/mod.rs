//! Source adapters: fetch and extraction (design doc 0001 §9; v0 ships the
//! platform-aware drop-in adapter only — see `platforms`).

use std::{path::Path, time::Duration};

use miette::{Context, IntoDiagnostic, Result, bail, miette};
use tracing::{debug, instrument, warn};
use url::Url;

use crate::domain::events::ExtractedFields;

pub mod extract;
pub mod platforms;

/// Politeness delay before every HTTP request (spec: 200–500ms).
const POLITENESS_DELAY: Duration = Duration::from_millis(300);
/// Bounded total request timeout: the writer lock is acquired after the
/// fetch, but a stalled endpoint must still not hang the CLI forever.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Cap on server-directed retry waits. Longer than this and we give up
/// rather than retry early (which would violate the control header).
const MAX_RETRY_DELAY: Duration = Duration::from_secs(30);
const DEFAULT_RETRY_DELAY: Duration = Duration::from_secs(2);
const MAX_RETRIES: u32 = 1;

/// A raw HTTP response, reduced to what the politeness layer needs.
#[derive(Clone, Debug)]
pub struct FetchResponse {
    pub status: u16,
    pub retry_after: Option<String>,
    pub body: String,
}

/// Fetch failures that must NOT trigger the API→HTML fallback. The fallback
/// exists for *shape* failures (unrecognized API responses); retrying a
/// rate-limited or unreachable host via its page URL would violate control
/// headers or just fail again — so these propagate. Non-rate-limit HTTP
/// statuses (404, 500, …) are ordinary outcomes and DO fall back: a dead
/// API endpoint says nothing about the page.
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum FetchError {
    #[error(
        "rate limited fetching {url}: server asked for a retry after {delay:?}, over the local cap"
    )]
    #[diagnostic(code(gwl_jobs::rate_limited))]
    RateLimited { url: String, delay: Duration },
    #[error("transport error fetching {url}: {message}")]
    #[diagnostic(code(gwl_jobs::transport))]
    Transport { url: String, message: String },
}

/// Transport seam. The good-citizen logic (politeness delay, retry
/// honoring `Retry-After`, status handling) lives in `PoliteClient` and is
/// tested against scripted `Fetcher` implementations — no network, no mock
/// server.
pub trait Fetcher {
    fn get(&self, url: &Url) -> impl Future<Output = Result<FetchResponse>> + Send;
}

/// Production transport: reqwest with a bounded timeout and an honest UA.
pub struct HttpFetcher {
    client: reqwest::Client,
}

impl HttpFetcher {
    pub fn new() -> Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent(concat!(
                env!("CARGO_PKG_NAME"),
                "/",
                env!("CARGO_PKG_VERSION"),
                " (+https://github.com/plathrop/gwl-job-tools)"
            ))
            .timeout(REQUEST_TIMEOUT)
            .build()
            .into_diagnostic()?;
        Ok(Self { client })
    }
}

impl Fetcher for HttpFetcher {
    async fn get(&self, url: &Url) -> Result<FetchResponse> {
        let transport = |e: reqwest::Error| FetchError::Transport {
            url: url.to_string(),
            message: e.to_string(),
        };
        let response = self
            .client
            .get(url.clone())
            .send()
            .await
            .map_err(|e| miette::Report::new(transport(e)))?;
        let status = response.status().as_u16();
        let retry_after = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let body = response
            .text()
            .await
            .map_err(|e| miette::Report::new(transport(e)))?;
        Ok(FetchResponse {
            status,
            retry_after,
            body,
        })
    }
}

/// HTTP client with good-citizen behavior baked in: politeness delay
/// between requests, redirect following (reqwest default, in `HttpFetcher`),
/// and one retry honoring `Retry-After` on 429/503.
pub struct PoliteClient<F: Fetcher> {
    fetcher: F,
    politeness_delay: Duration,
    max_retry_delay: Duration,
}

impl<F: Fetcher> PoliteClient<F> {
    pub fn new(fetcher: F) -> Self {
        Self {
            fetcher,
            politeness_delay: POLITENESS_DELAY,
            max_retry_delay: MAX_RETRY_DELAY,
        }
    }

    #[cfg(test)]
    fn with_delays(politeness: Duration, max_retry: Duration, fetcher: F) -> Self {
        Self {
            fetcher,
            politeness_delay: politeness,
            max_retry_delay: max_retry,
        }
    }

    async fn get(&self, url: &Url) -> Result<FetchResponse> {
        let mut attempt = 0;
        loop {
            // Be a good citizen: fixed delay before every request.
            tokio::time::sleep(self.politeness_delay).await;
            debug!(%url, attempt, "fetching");
            let response = self.fetcher.get(url).await?;
            let status = response.status;
            if (status == 429 || status == 503) && attempt < MAX_RETRIES {
                // Obey control headers: seconds or HTTP-date forms.
                let delay = parse_retry_after(response.retry_after.as_deref())
                    .unwrap_or(DEFAULT_RETRY_DELAY);
                if delay > self.max_retry_delay {
                    return Err(FetchError::RateLimited {
                        url: url.to_string(),
                        delay,
                    }
                    .into());
                }
                warn!(%url, ?delay, "rate-limited; honoring Retry-After");
                tokio::time::sleep(delay).await;
                attempt += 1;
                continue;
            }
            if !(200..300).contains(&status) {
                bail!("fetching {url} failed with status {status}");
            }
            return Ok(response);
        }
    }

    pub async fn get_text(&self, url: &Url) -> Result<String> {
        Ok(self.get(url).await?.body)
    }

    pub async fn get_json(&self, url: &Url) -> Result<serde_json::Value> {
        let body = self.get(url).await?.body;
        serde_json::from_str(&body)
            .into_diagnostic()
            .wrap_err_with(|| format!("parsing JSON from {url}"))
    }
}

/// `Retry-After` in either of its two forms: delay-seconds or HTTP-date.
fn parse_retry_after(value: Option<&str>) -> Option<Duration> {
    let value = value?.trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let date = httpdate::parse_http_date(value).ok()?;
    date.duration_since(std::time::SystemTime::now()).ok()
}

pub type HttpClient = PoliteClient<HttpFetcher>;

pub fn default_client() -> Result<HttpClient> {
    Ok(PoliteClient::new(HttpFetcher::new()?))
}

/// The result of fetching and extracting a posting, ready for identity
/// computation and the `ingested`/`updated` payload.
#[derive(Clone, Debug)]
pub struct IngestOutcome {
    pub source: String,
    pub url: Option<String>,
    pub raw_text: String,
    pub extracted: ExtractedFields,
}

/// Ingest a posting URL: public JSON API first for known boards, HTML fetch
/// + main-text extraction as the fallback (unknown sites, API failures with
/// unrecognized response shapes).
#[instrument(skip(http), fields(url = %url))]
pub async fn ingest_url<F: Fetcher>(url: &Url, http: &PoliteClient<F>) -> Result<IngestOutcome> {
    if let Some(platform) = platforms::detect(url) {
        match ingest_via_api(url, platform, http).await {
            Ok(outcome) => return Ok(outcome),
            Err(err) => {
                // Only *shape* failures fall back to HTML. Rate-limit and
                // transport failures propagate: fetching the same host's
                // page after a 429 would violate the control header we just
                // honored, and an unreachable host will not answer either
                // way. (Non-rate-limit statuses are NOT FetchErrors and do
                // fall back — a dead API endpoint says nothing about the
                // page.)
                if err.downcast_ref::<FetchError>().is_some() {
                    return Err(err);
                }
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

async fn ingest_via_api<F: Fetcher>(
    url: &Url,
    platform: platforms::Platform,
    http: &PoliteClient<F>,
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

async fn ingest_via_html<F: Fetcher>(url: &Url, http: &PoliteClient<F>) -> Result<IngestOutcome> {
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
    // Company fallback so the blacklist gate holds on the HTML path too.
    extracted.company = extracted
        .title
        .as_deref()
        .and_then(extract::company_from_title);
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
        // Plain-text drops have no title element; the first non-empty line
        // is the title candidate ("Staff Engineer — Acme" covers the
        // company fallback's needs). Markdown heading markers (`#`) are
        // stripped so a markdown drop and a plain-text drop of the same
        // posting hash to the same `tc:` dedupe key.
        let first_line = content
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .map(|l| l.trim_start_matches('#').trim_start().to_string());
        (first_line, content.to_string())
    };
    if raw_text.trim().is_empty() {
        bail!("no text could be extracted from {}", path.display());
    }

    let mut extracted = extract::extract_fields(&raw_text, None);
    extracted.title = title;
    extracted.req_id = extract::extract_req_id(&raw_text);
    // Company fallback so the blacklist gate holds on file drops too.
    extracted.company = extracted
        .title
        .as_deref()
        .and_then(extract::company_from_title);
    Ok(IngestOutcome {
        source: "drop-in".into(),
        url: None,
        raw_text,
        extracted,
    })
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, path::PathBuf, sync::Mutex};

    use super::*;

    // ── Scripted Fetcher seam ────────────────────────────────────

    struct ScriptedFetcher {
        responses: Mutex<VecDeque<FetchResponse>>,
        calls: Mutex<Vec<String>>,
    }

    impl ScriptedFetcher {
        fn with(responses: Vec<FetchResponse>) -> Self {
            Self {
                responses: Mutex::new(responses.into()),
                calls: Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    fn response(status: u16, retry_after: Option<&str>, body: &str) -> FetchResponse {
        FetchResponse {
            status,
            retry_after: retry_after.map(str::to_string),
            body: body.into(),
        }
    }

    impl Fetcher for ScriptedFetcher {
        async fn get(&self, url: &Url) -> Result<FetchResponse> {
            self.calls.lock().unwrap().push(url.to_string());
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| miette!("scripted fetcher ran out of responses"))
        }
    }

    fn client_with(fetcher: ScriptedFetcher) -> PoliteClient<ScriptedFetcher> {
        PoliteClient::with_delays(Duration::ZERO, Duration::from_secs(30), fetcher)
    }

    // ── Good-citizen behavior ────────────────────────────────────

    #[tokio::test(start_paused = true)]
    async fn retry_after_seconds_waits_and_retries_once() {
        let fetcher = ScriptedFetcher::with(vec![
            response(429, Some("2"), ""),
            response(200, None, "ok"),
        ]);
        let client = client_with(fetcher);
        let start = tokio::time::Instant::now();
        let url = Url::parse("https://example.com/j").unwrap();

        let text = client.get_text(&url).await.unwrap();

        assert_eq!(text, "ok");
        assert_eq!(client.fetcher.calls().len(), 2);
        assert!(start.elapsed() >= Duration::from_secs(2));
    }

    #[tokio::test(start_paused = true)]
    async fn retry_after_http_date_is_honored() {
        let date = httpdate::fmt_http_date(std::time::SystemTime::now() + Duration::from_secs(3));
        let fetcher = ScriptedFetcher::with(vec![
            response(503, Some(&date), ""),
            response(200, None, "ok"),
        ]);
        let client = client_with(fetcher);
        let url = Url::parse("https://example.com/j").unwrap();

        let text = client.get_text(&url).await.unwrap();

        assert_eq!(text, "ok");
        assert_eq!(client.fetcher.calls().len(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn second_rate_limit_gives_up() {
        let fetcher = ScriptedFetcher::with(vec![response(429, None, ""), response(429, None, "")]);
        let client = client_with(fetcher);
        let url = Url::parse("https://example.com/j").unwrap();

        assert!(client.get_text(&url).await.is_err());
        assert_eq!(client.fetcher.calls().len(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn non_retryable_status_errors_without_retrying() {
        let fetcher = ScriptedFetcher::with(vec![response(404, None, "not found")]);
        let client = client_with(fetcher);
        let url = Url::parse("https://example.com/j").unwrap();

        assert!(client.get_text(&url).await.is_err());
        assert_eq!(client.fetcher.calls().len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn retry_after_over_cap_bails_instead_of_retrying_early() {
        let fetcher = ScriptedFetcher::with(vec![response(429, Some("120"), "")]);
        let client = client_with(fetcher);
        let url = Url::parse("https://example.com/j").unwrap();

        assert!(client.get_text(&url).await.is_err());
        assert_eq!(client.fetcher.calls().len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn politeness_delay_applies_before_every_request() {
        let fetcher =
            ScriptedFetcher::with(vec![response(200, None, "a"), response(200, None, "b")]);
        let client =
            PoliteClient::with_delays(Duration::from_millis(300), Duration::from_secs(30), fetcher);
        let start = tokio::time::Instant::now();
        let url = Url::parse("https://example.com/j").unwrap();

        client.get_text(&url).await.unwrap();
        client.get_text(&url).await.unwrap();

        assert!(start.elapsed() >= Duration::from_millis(600));
    }

    // ── API → HTML fallback orchestration ────────────────────────

    #[tokio::test(start_paused = true)]
    async fn unrecognized_api_response_falls_back_to_html() {
        let posting = Url::parse("https://job-boards.greenhouse.io/acme/jobs/123").unwrap();
        let html = "<html><head><title>Engineer — Acme</title></head><body>\
                    <article><h1>Platform Engineer</h1>\
                    <p>Remote role building platforms with a wonderful team of \
                    experienced engineers who care deeply about reliability, \
                    operability, and mentoring across the organization daily.</p>\
                    </article></body></html>";
        let fetcher = ScriptedFetcher::with(vec![
            // API returns a valid-JSON body missing every expected field.
            response(200, None, "{}"),
            response(200, None, html),
        ]);
        let client = client_with(fetcher);

        let outcome = ingest_url(&posting, &client).await.unwrap();

        assert_eq!(outcome.source, "drop-in");
        let calls = client.fetcher.calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(
            calls[0],
            "https://boards-api.greenhouse.io/v1/boards/acme/jobs/123"
        );
        assert_eq!(calls[1], posting.to_string());
        assert!(outcome.raw_text.contains("Platform Engineer"));
    }

    #[tokio::test(start_paused = true)]
    async fn rate_limited_api_does_not_fall_back_to_same_host() {
        // A 429 whose Retry-After exceeds the local cap must propagate —
        // fetching the posting page on the SAME host after the server told
        // us to back off would violate the control header.
        let posting =
            Url::parse("https://nvidia.wd5.myworkdayjobs.com/en-US/Site/job/X_JR1").unwrap();
        let fetcher = ScriptedFetcher::with(vec![response(429, Some("120"), "")]);
        let client = client_with(fetcher);

        let result = ingest_url(&posting, &client).await;

        assert!(result.is_err());
        assert_eq!(client.fetcher.calls().len(), 1);
    }

    // ── parse_retry_after ────────────────────────────────────────

    #[test]
    fn parses_retry_after_seconds() {
        assert_eq!(parse_retry_after(Some("7")), Some(Duration::from_secs(7)));
    }

    #[test]
    fn parses_retry_after_http_date() {
        let date = httpdate::fmt_http_date(std::time::SystemTime::now() + Duration::from_secs(5));
        let parsed = parse_retry_after(Some(&date)).unwrap();
        assert!(parsed <= Duration::from_secs(5));
        assert!(parsed > Duration::from_secs(3));
    }

    #[test]
    fn unparseable_retry_after_is_none() {
        assert_eq!(parse_retry_after(Some("garbage")), None);
        assert_eq!(parse_retry_after(None), None);
    }

    // ── file drops ───────────────────────────────────────────────

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
    fn file_drop_markdown_strips_heading_markers() {
        // A markdown drop and a plain-text drop of the same posting must
        // hash to the same `tc:` dedupe key — the `#` heading marker must
        // not leak into the title.
        let outcome = ingest_file(
            &PathBuf::from("jd.md"),
            "# Staff Software Engineer, Platform — Acme\n\nRemote. Salary: $200,000 - $250,000.",
        )
        .unwrap();
        assert_eq!(
            outcome.extracted.title.as_deref(),
            Some("Staff Software Engineer, Platform — Acme")
        );
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
