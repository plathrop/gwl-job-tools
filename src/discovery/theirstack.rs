//! TheirStack Jobs API adapter (decision 0012, OpenSpec change
//! `theirstack-paid-feed`): a rich, paid source. Each record carries the full
//! description (markdown) plus structured salary/remote/technology fields, so
//! the adapter maps records directly to [`IngestOutcome`] — no re-fetch of the
//! canonical page (the snapshot path).

use std::{future::Future, pin::Pin};

use miette::{Result, bail};
use serde_json::{Value, json};
use tracing::debug;
use url::Url;

use super::{DiscoverySource, SourceBatch};
use crate::{
    config::{Config, SourceConfig},
    domain::events::{CompRange, ExtractedFields},
    ingest::{self, Fetcher, HttpClient, IngestOutcome, PoliteClient},
};

const THEIRSTACK_API: &str = "https://api.theirstack.com/v1/jobs/search";
/// TheirStack's maximum page size (decision 0012).
const PAGE_SIZE: u64 = 500;

pub struct Theirstack {
    api_url: String,
    api_key: String,
    posted_at_max_age_days: u64,
    strict_filtering: bool,
    blacklist: Vec<String>,
    remote_only: bool,
    compensation_floor: Option<u64>,
    discovered_at_gte: Option<String>,
}

impl Theirstack {
    pub fn new(source: &SourceConfig, config: &Config, discovered_at_gte: Option<String>) -> Self {
        Self {
            api_url: THEIRSTACK_API.into(),
            api_key: source.api_key.clone(),
            posted_at_max_age_days: source.posted_at_max_age_days,
            strict_filtering: source.strict_filtering,
            blacklist: config.blacklist.clone(),
            remote_only: config.remote_only,
            compensation_floor: config.compensation_floor,
            discovered_at_gte,
        }
    }

    /// Build the search query body (pure, testable — design decisions 3 & 4):
    /// blacklist → `company_name_not`/`company_domain_not`, recency →
    /// `posted_at_max_age_days` always, and behind `strict_filtering` also
    /// `remote_only` → `workplace_types_or: ["remote"]` and `compensation_floor`
    /// → `min_salary_usd`. Skills are never a query filter.
    fn build_query(&self) -> Value {
        let mut query = serde_json::Map::new();
        query.insert(
            "posted_at_max_age_days".into(),
            json!(self.posted_at_max_age_days),
        );
        query.insert("limit".into(), json!(PAGE_SIZE));

        // Best-effort server-side blacklist pre-filter (the client gate is the
        // backstop): domain-looking entries → `company_domain_not`, names →
        // `company_name_not` (exact, case-sensitive).
        let mut domains = Vec::new();
        let mut names = Vec::new();
        for entry in &self.blacklist {
            if entry.contains('.') && !entry.chars().any(char::is_whitespace) {
                domains.push(Value::String(entry.clone()));
            } else {
                names.push(Value::String(entry.clone()));
            }
        }
        if !domains.is_empty() {
            query.insert("company_domain_not".into(), Value::Array(domains));
        }
        if !names.is_empty() {
            query.insert("company_name_not".into(), Value::Array(names));
        }

        if self.strict_filtering {
            if self.remote_only {
                query.insert("workplace_types_or".into(), json!(["remote"]));
            }
            if let Some(floor) = self.compensation_floor {
                query.insert("min_salary_usd".into(), json!(floor));
            }
        }

        if let Some(watermark) = &self.discovered_at_gte {
            query.insert("discovered_at_gte".into(), json!(watermark));
        }

        Value::Object(query)
    }

    /// Parse a search response into jobs plus the truncation count.
    fn parse_response(value: &Value) -> Result<TheirstackResponse> {
        let data = value.get("data").and_then(Value::as_array).ok_or_else(|| {
            miette::miette!("theirstack response missing data array (shape changed?)")
        })?;
        let jobs = data.iter().map(parse_job).collect();
        let truncated_results = value
            .get("metadata")
            .and_then(|m| m.get("truncated_results"))
            .and_then(num_to_u64)
            .unwrap_or(0);
        Ok(TheirstackResponse {
            jobs,
            truncated_results,
        })
    }
}

/// One parsed TheirStack record, before mapping to [`IngestOutcome`].
#[derive(Clone, Debug)]
struct TheirstackJob {
    url: Option<String>,
    description: String,
    title: Option<String>,
    company: Option<String>,
    location: Option<String>,
    comp: Option<CompRange>,
    remote: Option<bool>,
    /// Parsed for observability and the eventual skills-scoring enrichment;
    /// the v0 skills dimension still matches resume keywords against
    /// `raw_text`, so this is not yet consumed (asserted by tests).
    #[allow(dead_code)]
    technology_slugs: Vec<String>,
    discovered_at: Option<String>,
}

#[derive(Debug)]
struct TheirstackResponse {
    jobs: Vec<TheirstackJob>,
    truncated_results: u64,
}

impl TheirstackJob {
    fn into_outcome(self) -> IngestOutcome {
        IngestOutcome {
            adapter: "theirstack".into(),
            source: "theirstack".into(),
            url: self.url,
            raw_text: self.description,
            extracted: ExtractedFields {
                title: self.title,
                company: self.company,
                location: self.location,
                remote: self.remote,
                comp: self.comp,
                ..Default::default()
            },
        }
    }
}

impl DiscoverySource for Theirstack {
    fn fetch<'a>(
        &'a self,
        client: &'a HttpClient,
    ) -> Pin<Box<dyn Future<Output = Result<SourceBatch>> + Send + 'a>> {
        Box::pin(async move {
            // A missing key fails the source (reported; run continues), never
            // a silent skip.
            if self.api_key.is_empty() {
                bail!(
                    "theirstack api_key is empty (set THEIRSTACK_API_KEY or [sources.theirstack].api_key)"
                );
            }
            let query = self.build_query();
            let api_url = Url::parse(&self.api_url).expect("static API URL parses");
            let (jobs, truncated_results) =
                fetch_jobs(client, &api_url, &query, &self.api_key).await?;
            let discovered_at = max_discovered_at(&jobs);
            let mut outcomes = Vec::with_capacity(jobs.len());
            for job in jobs {
                emit_mismatches(&job);
                outcomes.push(job.into_outcome());
            }
            Ok(SourceBatch {
                outcomes,
                failed: 0,
                truncated_results,
                discovered_at,
            })
        })
    }

    fn charges_per_record(&self) -> bool {
        true
    }
}

/// Paginate the search endpoint (serial, `page`-based) until a page returns
/// fewer than the page size. Generic over the transport seam so tests can
/// script the responses.
async fn fetch_jobs<F: Fetcher>(
    client: &PoliteClient<F>,
    api_url: &Url,
    query: &Value,
    api_key: &str,
) -> Result<(Vec<TheirstackJob>, u64)> {
    let mut jobs = Vec::new();
    let mut truncated_results = 0u64;
    let mut page = 0u64;
    loop {
        let mut page_query = query.clone();
        if let Value::Object(ref mut map) = page_query {
            map.insert("page".into(), json!(page));
        }
        let response = client
            .post_json(api_url, &page_query, Some(api_key))
            .await?;
        let parsed = Theirstack::parse_response(&response)?;
        truncated_results = truncated_results.max(parsed.truncated_results);
        let count = parsed.jobs.len();
        jobs.extend(parsed.jobs);
        if count < PAGE_SIZE as usize {
            break;
        }
        page += 1;
    }
    Ok((jobs, truncated_results))
}

fn parse_job(record: &Value) -> TheirstackJob {
    let description = record
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let title = record
        .get("job_title")
        .and_then(Value::as_str)
        .map(str::to_string);
    let company = record
        .get("company")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            record
                .get("company_object")
                .and_then(|o| o.get("name"))
                .and_then(Value::as_str)
                .map(str::to_string)
        });
    let location = record
        .get("location")
        .and_then(Value::as_str)
        .map(str::to_string);
    let technology_slugs = record
        .get("technology_slugs")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();

    TheirstackJob {
        url: record
            .get("final_url")
            .and_then(Value::as_str)
            .or_else(|| record.get("url").and_then(Value::as_str))
            .and_then(|u| Url::parse(u).ok())
            .map(|u| crate::domain::identity::canonicalize_url(&u)),
        description,
        title,
        company,
        location,
        comp: parse_comp(record),
        remote: parse_remote(record),
        technology_slugs,
        discovered_at: record
            .get("discovered_at")
            .and_then(Value::as_str)
            .map(str::to_string),
    }
}

fn parse_comp(record: &Value) -> Option<CompRange> {
    let min = record
        .get("min_annual_salary_usd")
        .and_then(num_to_u64)
        .or_else(|| record.get("min_annual_salary").and_then(num_to_u64));
    let max = record
        .get("max_annual_salary_usd")
        .and_then(num_to_u64)
        .or_else(|| record.get("max_annual_salary").and_then(num_to_u64));
    if min.is_none() && max.is_none() {
        return None;
    }
    let currency = record
        .get("salary_currency")
        .and_then(Value::as_str)
        .unwrap_or("USD")
        .to_string();
    Some(CompRange {
        min,
        max,
        currency,
        period: "year".into(),
        raw: record
            .get("salary_string")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    })
}

fn parse_remote(record: &Value) -> Option<bool> {
    let remote = record.get("remote").and_then(Value::as_bool);
    let hybrid = record.get("hybrid").and_then(Value::as_bool);
    match (remote, hybrid) {
        (Some(true), _) => Some(true),
        (_, Some(true)) => Some(false), // hybrid is confident non-remote
        (Some(false), _) => Some(false), // explicitly not remote
        _ => None,
    }
}

/// A number field, tolerant of floats that represent whole dollars.
fn num_to_u64(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_f64().map(|f| f.round() as u64))
}

/// Trust-but-verify (design decision 2): run our own text heuristics over the
/// description and log a `debug!` per disagreement with the structured fields.
/// Never blocking — it exists to make TheirStack data problems visible.
fn mismatches(job: &TheirstackJob) -> Vec<&'static str> {
    let ours = ingest::extract::extract_fields(&job.description, job.location.as_deref());
    let mut out = Vec::new();
    if job.comp.is_some() && ours.comp != job.comp {
        out.push("comp");
    }
    if job.remote.is_some() && ours.remote != job.remote {
        out.push("remote");
    }
    out
}

fn emit_mismatches(job: &TheirstackJob) {
    for field in mismatches(job) {
        debug!(
            field,
            source = "theirstack",
            url = ?job.url,
            "theirstack structured field disagrees with text heuristics"
        );
    }
}

/// Normalize TheirStack's naive `discovered_at` (already UTC per the API docs)
/// to an explicit-UTC timestamp for the watermark.
fn normalize_discovered_at(raw: &str) -> String {
    let raw = raw.trim();
    if raw.ends_with('Z') || raw.contains('+') {
        raw.to_string()
    } else {
        format!("{raw}Z")
    }
}

/// The max `discovered_at` across a fetch (the watermark cursor's lower
/// bound for the next run). Normalized strings are same-precision RFC 3339,
/// so lexicographic order is chronological order.
fn max_discovered_at(jobs: &[TheirstackJob]) -> Option<String> {
    jobs.iter()
        .filter_map(|j| j.discovered_at.as_deref())
        .map(normalize_discovered_at)
        .max()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source_config(api_key: &str) -> SourceConfig {
        SourceConfig {
            enabled: true,
            api_key: api_key.into(),
            ..Default::default()
        }
    }

    fn config() -> Config {
        Config {
            blacklist: vec!["salesforce".into(), "google.com".into()],
            remote_only: true,
            compensation_floor: Some(180_000),
            ..Default::default()
        }
    }

    fn adapter(api_key: &str) -> Theirstack {
        Theirstack::new(&source_config(api_key), &config(), None)
    }

    fn fixture_job() -> Value {
        serde_json::from_str(
            r#"{
                "job_title": "Senior Data Engineer",
                "description": "We are hiring a Senior Data Engineer. Remote. Salary: $200,000 - $250,000.",
                "company": "Acme",
                "company_domain": "acme.com",
                "final_url": "https://acme.com/careers/job/123?utm_source=x",
                "location": "Remote, US",
                "min_annual_salary_usd": 200000,
                "max_annual_salary_usd": 250000,
                "salary_currency": "USD",
                "salary_string": "$200,000 - $250,000",
                "remote": true,
                "technology_slugs": ["postgresql", "kafka"],
                "discovered_at": "2024-01-01T00:00:00"
            }"#,
        )
        .unwrap()
    }

    // ── query shaping (task 2.4) ─────────────────────────────────

    #[test]
    fn default_query_carries_blacklist_and_recency_only() {
        let query = adapter("k").build_query();
        assert_eq!(query["posted_at_max_age_days"], 30);
        assert_eq!(query["limit"], 500);
        // Domain-looking blacklist entry → company_domain_not; name → name.
        assert_eq!(query["company_domain_not"], json!(["google.com"]));
        assert_eq!(query["company_name_not"], json!(["salesforce"]));
        // Strict off: no remote/comp filters.
        assert!(query.get("workplace_types_or").is_none());
        assert!(query.get("min_salary_usd").is_none());
        // No watermark.
        assert!(query.get("discovered_at_gte").is_none());
    }

    #[test]
    fn strict_query_adds_remote_and_comp_filters() {
        let mut source = source_config("k");
        source.strict_filtering = true;
        let adapter = Theirstack::new(&source, &config(), None);
        let query = adapter.build_query();
        assert_eq!(query["workplace_types_or"], json!(["remote"]));
        assert_eq!(query["min_salary_usd"], json!(180_000));
    }

    #[test]
    fn query_includes_watermark_when_provided() {
        let adapter = Theirstack::new(
            &source_config("k"),
            &config(),
            Some("2024-01-01T00:00:00Z".into()),
        );
        assert_eq!(
            adapter.build_query()["discovered_at_gte"],
            json!("2024-01-01T00:00:00Z")
        );
    }

    #[test]
    fn skills_never_shape_the_query() {
        let query = adapter("k").build_query();
        assert!(query.get("job_technology_slug_or").is_none());
        assert!(query.get("job_technology_slug_and").is_none());
    }

    // ── parsing (task 2.2) ───────────────────────────────────────

    #[test]
    fn parses_fixture_fields() {
        let value = serde_json::json!({
            "data": [fixture_job()],
            "metadata": {"truncated_results": 0}
        });
        let response = Theirstack::parse_response(&value).unwrap();
        assert_eq!(response.jobs.len(), 1);
        let job = &response.jobs[0];
        assert_eq!(job.title.as_deref(), Some("Senior Data Engineer"));
        assert_eq!(job.company.as_deref(), Some("Acme"));
        assert_eq!(job.remote, Some(true));
        assert_eq!(job.technology_slugs, vec!["postgresql", "kafka"]);
        assert_eq!(job.url.as_deref(), Some("https://acme.com/careers/job/123"));
        let comp = job.comp.as_ref().unwrap();
        assert_eq!(comp.min, Some(200_000));
        assert_eq!(comp.max, Some(250_000));
        assert!(job.description.contains("Senior Data Engineer"));
    }

    #[test]
    fn parses_truncated_results() {
        let value = serde_json::json!({
            "data": [],
            "metadata": {"truncated_results": 7}
        });
        let response = Theirstack::parse_response(&value).unwrap();
        assert_eq!(response.truncated_results, 7);
    }

    #[test]
    fn missing_data_array_errors() {
        assert!(Theirstack::parse_response(&serde_json::json!({})).is_err());
    }

    // ── mapping to IngestOutcome (task 2.3) ───────────────────────

    #[test]
    fn fixture_maps_to_expected_outcome() {
        let value = serde_json::json!({ "data": [fixture_job()] });
        let response = Theirstack::parse_response(&value).unwrap();
        let outcome = response.jobs.into_iter().next().unwrap().into_outcome();
        assert_eq!(outcome.adapter, "theirstack");
        assert_eq!(outcome.source, "theirstack");
        assert_eq!(
            outcome.url.as_deref(),
            Some("https://acme.com/careers/job/123")
        );
        assert!(outcome.raw_text.contains("Senior Data Engineer"));
        assert_eq!(
            outcome.extracted.title.as_deref(),
            Some("Senior Data Engineer")
        );
        assert_eq!(outcome.extracted.company.as_deref(), Some("Acme"));
        assert_eq!(outcome.extracted.remote, Some(true));
        assert_eq!(outcome.extracted.comp.unwrap().min, Some(200_000));
    }

    #[tokio::test]
    async fn missing_key_returns_err() {
        let adapter = adapter("");
        let client = crate::ingest::default_client().unwrap();
        let result = adapter.fetch(&client).await;
        assert!(result.is_err());
    }

    // ── trust-but-verify (task 2.5) ──────────────────────────────

    #[test]
    fn mismatch_detects_salary_disagreement() {
        // Structured comp is $200k-$250k; the description quotes a
        // different range, so our heuristics disagree.
        let job = parse_job(&fixture_job());
        // Give the fixture a description that quotes a different salary.
        let mut job = job;
        job.description = "Salary: $300,000 - $350,000. Remote.".into();
        let ours = mismatches(&job);
        assert!(ours.contains(&"comp"));
    }

    #[test]
    fn no_mismatch_when_fields_agree() {
        let job = parse_job(&fixture_job());
        // The fixture's description quotes $200k-$250k and "Remote", matching
        // the structured fields.
        assert!(mismatches(&job).is_empty());
    }

    // ── watermark normalization ──────────────────────────────────

    #[test]
    fn normalizes_naive_discovered_at_to_utc() {
        assert_eq!(
            normalize_discovered_at("2024-01-01T00:00:00"),
            "2024-01-01T00:00:00Z"
        );
        assert_eq!(
            normalize_discovered_at("2024-01-01T00:00:00Z"),
            "2024-01-01T00:00:00Z"
        );
    }

    #[test]
    fn max_discovered_at_takes_latest() {
        let jobs = vec![
            TheirstackJob {
                discovered_at: Some("2024-01-01T00:00:00".into()),
                ..default_job()
            },
            TheirstackJob {
                discovered_at: Some("2024-01-02T00:00:00".into()),
                ..default_job()
            },
        ];
        assert_eq!(
            max_discovered_at(&jobs).as_deref(),
            Some("2024-01-02T00:00:00Z")
        );
    }

    fn default_job() -> TheirstackJob {
        TheirstackJob {
            url: None,
            description: String::new(),
            title: None,
            company: None,
            location: None,
            comp: None,
            remote: None,
            technology_slugs: vec![],
            discovered_at: None,
        }
    }
}
