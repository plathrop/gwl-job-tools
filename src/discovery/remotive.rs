//! Remotive: a free, deterministic JSON feed of remote job postings
//! (OpenSpec change `discovery-ingestion`). The feed URL is a proxy that
//! redirects to the underlying ATS; the driver resolves it via
//! [`crate::ingest::ingest_url`].

use std::{future::Future, pin::Pin};

use miette::{Context, IntoDiagnostic, Result, miette};
use serde_json::Value;
use tracing::debug;
use url::Url;

use super::{DiscoverySource, Posting};
use crate::ingest::{HttpClient, extract};

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

    /// Parse a Remotive API response body into postings (pure, testable).
    pub fn parse(body: &str) -> Result<Vec<Posting>> {
        let json: Value = serde_json::from_str(body).into_diagnostic()?;
        let jobs = json
            .get("jobs")
            .and_then(Value::as_array)
            .ok_or_else(|| miette!("remotive API response missing jobs array (shape changed?)"))?;
        let mut postings = Vec::with_capacity(jobs.len());
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
            postings.push(Posting {
                url,
                title: job.get("title").and_then(Value::as_str).map(str::to_string),
                company: job
                    .get("company_name")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                comp: job
                    .get("salary")
                    .and_then(Value::as_str)
                    .and_then(extract::extract_comp),
                location: job
                    .get("candidate_required_location")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                remote: None,
                req_id: None,
            });
        }
        if skipped > 0 {
            debug!(
                skipped,
                "remotive postings skipped (missing or unparseable url)"
            );
        }
        Ok(postings)
    }
}

impl Default for Remotive {
    fn default() -> Self {
        Self::new()
    }
}

impl DiscoverySource for Remotive {
    fn postings<'a>(
        &'a self,
        client: &'a HttpClient,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Posting>>> + Send + 'a>> {
        Box::pin(async move {
            let url = Url::parse(&self.api_url).expect("static API URL parses");
            let body = client
                .get_text(&url)
                .await
                .wrap_err_with(|| format!("fetching Remotive feed {url}"))?;
            Self::parse(&body)
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
                }
            ]
        }"#;

        let postings = Remotive::parse(body).unwrap();

        assert_eq!(postings.len(), 1);
        let p = &postings[0];
        assert_eq!(p.title.as_deref(), Some("Senior Rust Engineer"));
        assert_eq!(p.company.as_deref(), Some("Acme"));
        assert_eq!(p.location.as_deref(), Some("Worldwide"));
        assert_eq!(
            p.url.as_str(),
            "https://remotive.com/remote-jobs/software-dev/123"
        );
        let comp = p.comp.as_ref().unwrap();
        assert_eq!(comp.min, Some(150_000));
        assert_eq!(comp.max, Some(200_000));
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
