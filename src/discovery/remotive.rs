//! Remotive: a free, deterministic JSON feed of remote job postings
//! (OpenSpec change `discovery-ingestion`). The feed URL is a proxy that
//! redirects to the underlying ATS; the adapter resolves each URL via
//! [`crate::ingest::ingest_url`] — the thin (re-extract) case.

use std::{future::Future, pin::Pin};

use miette::{Context, IntoDiagnostic, Result, miette};
use serde_json::Value;
use tracing::debug;
use url::Url;

use super::{DiscoverySource, SourceBatch, ingest_postings};
use crate::ingest::HttpClient;

const REMOTIVE_API: &str = "https://remotive.com/api/remote-jobs";

pub struct Remotive {
    api_url: String,
}

impl Remotive {
    pub fn new() -> Self {
        Self {
            api_url: REMOTIVE_API.into(),
        }
    }

    /// Parse a Remotive API response body into posting URLs (pure, testable).
    pub fn parse(body: &str) -> Result<Vec<Url>> {
        let json: Value = serde_json::from_str(body).into_diagnostic()?;
        let jobs = json
            .get("jobs")
            .and_then(Value::as_array)
            .ok_or_else(|| miette!("remotive API response missing jobs array (shape changed?)"))?;
        let mut urls = Vec::with_capacity(jobs.len());
        let mut skipped = 0;
        for job in jobs {
            let Some(url) = job.get("url").and_then(Value::as_str) else {
                skipped += 1;
                continue;
            };
            let Ok(url) = Url::parse(url) else {
                skipped += 1;
                continue;
            };
            urls.push(url);
        }
        if skipped > 0 {
            debug!(
                skipped,
                "remotive postings skipped (missing or unparseable url)"
            );
        }
        Ok(urls)
    }
}

impl Default for Remotive {
    fn default() -> Self {
        Self::new()
    }
}

impl DiscoverySource for Remotive {
    fn fetch<'a>(
        &'a self,
        client: &'a HttpClient,
    ) -> Pin<Box<dyn Future<Output = Result<SourceBatch>> + Send + 'a>> {
        Box::pin(async move {
            let url = Url::parse(&self.api_url).expect("static API URL parses");
            let body = client
                .get_text(&url)
                .await
                .wrap_err_with(|| format!("fetching Remotive feed {url}"))?;
            let postings = Self::parse(&body)?;
            Ok(ingest_postings(postings, client).await)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_remotive_feed_fixture() {
        let body = r#"{
            "jobs": [
                {
                    "id": 123,
                    "url": "https://remotive.com/remote-jobs/software-dev/123",
                    "title": "Senior Rust Engineer",
                    "company_name": "Acme",
                    "category": "Software Development",
                    "job_type": "full_time",
                    "publication_date": "2026-09-01T00:00:00",
                    "candidate_required_location": "Worldwide",
                    "salary": "$150,000 - $200,000 USD",
                    "description": "<p>Build things.</p>",
                    "tags": []
                },
                {
                    "id": 124,
                    "url": "not-a-url",
                    "title": "Broken"
                }
            ]
        }"#;

        let urls = Remotive::parse(body).unwrap();

        // The broken entry is skipped (not fatal); the valid one parses.
        assert_eq!(urls.len(), 1);
        assert_eq!(
            urls[0].as_str(),
            "https://remotive.com/remote-jobs/software-dev/123"
        );
    }

    #[test]
    fn missing_jobs_array_errors() {
        assert!(Remotive::parse("{}").is_err());
    }

    #[test]
    fn malformed_json_errors() {
        assert!(Remotive::parse("not json").is_err());
    }
}
