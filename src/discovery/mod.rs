//! Discovery layer: pluggable feed sources that yield job postings for the
//! ingest pipeline (OpenSpec change `discovery-ingestion`; extended by
//! `theirstack-paid-feed`).
//!
//! A [`DiscoverySource`] yields an already-extracted [`SourceBatch`]: either
//! postings it mapped directly from a rich feed (TheirStack) or URLs it
//! re-extracted itself via [`crate::ingest::ingest_url`] (Remotive). The
//! driver no longer distinguishes the two shapes — an adapter owns "do I
//! re-extract or not".

pub mod remotive;
pub mod theirstack;

use std::{collections::HashMap, future::Future, pin::Pin};

use miette::{Result, miette};
use tracing::{Instrument, debug, instrument, warn};
use url::Url;

use crate::{
    config::Config,
    ingest::{self, Fetcher, HttpClient, IngestOutcome, PoliteClient},
};

/// One source's fetch result: the already-extracted postings, the count of
/// postings whose fetch/extract failed (`failed`, non-fatal), the credit
/// truncation count, and the max `discovered_at` watermark.
#[derive(Debug)]
pub struct SourceBatch {
    pub outcomes: Vec<IngestOutcome>,
    pub failed: u64,
    /// Credit exhaustion: results the source could not return (TheirStack's
    /// `metadata.truncated_results`). `0` for sources without credits.
    pub truncated_results: u64,
    /// The max `discovered_at` (UTC-normalized) seen in this fetch, for the
    /// watermark cursor. `None` for sources without one (Remotive).
    pub discovered_at: Option<String>,
}

/// A feed source: fetch its already-extracted postings. Feed-agnostic — free
/// curated feeds (Remotive/WWR) now, paid commercial APIs (TheirStack) later.
///
/// The boxed-future return (rather than native `async fn`) keeps the trait
/// dyn-compatible, so sources can be held as `Box<dyn DiscoverySource>`.
pub trait DiscoverySource {
    fn fetch<'a>(
        &'a self,
        client: &'a HttpClient,
    ) -> Pin<Box<dyn Future<Output = Result<SourceBatch>> + Send + 'a>>;

    /// Whether fetching this source spends credits per record (default: no).
    /// Drives the `--dry-run` gate on paid sources.
    fn charges_per_record(&self) -> bool {
        false
    }
}

/// One source's fetch result; `Err` means the whole source failed (feed
/// down, malformed response, timeout) — non-fatal for the run.
#[derive(Debug)]
pub struct SourceFetch {
    pub source: String,
    pub batch: Result<SourceBatch>,
}

/// Null-rate facts about a source's fetched outcomes (theirstack-paid-feed
/// observability): how many returned records had unknown workplace / salary,
/// and how many strict mode would have dropped.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct NullRates {
    pub unknown_workplace: u64,
    pub unknown_salary: u64,
    pub strict_would_drop: u64,
}

/// Compute null-rate facts over a set of fetched outcomes.
pub fn null_rates<'a, I>(outcomes: I) -> NullRates
where
    I: IntoIterator<Item = &'a IngestOutcome>,
{
    let mut rates = NullRates::default();
    for outcome in outcomes {
        if outcome.extracted.remote.is_none() {
            rates.unknown_workplace += 1;
        }
        if outcome.extracted.comp.is_none() {
            rates.unknown_salary += 1;
        }
        if outcome.extracted.remote.is_none() || outcome.extracted.comp.is_none() {
            rates.strict_would_drop += 1;
        }
    }
    rates
}

/// Fetch every source; a source whose fetch fails yields `Err` and the run
/// continues with the remaining sources (spec: "A failed source does not
/// abort the run").
#[instrument(skip_all)]
pub async fn fetch_sources(
    sources: &[(String, Box<dyn DiscoverySource>)],
    client: &HttpClient,
) -> Vec<SourceFetch> {
    let mut fetches = Vec::with_capacity(sources.len());
    for (name, source) in sources {
        let span = tracing::info_span!(
            "source_fetch",
            source = %name,
            unknown_workplace = tracing::field::Empty,
            unknown_salary = tracing::field::Empty,
            strict_would_drop = tracing::field::Empty,
            truncated_results = tracing::field::Empty,
        );
        match source.fetch(client).instrument(span.clone()).await {
            Ok(batch) => {
                let rates = null_rates(batch.outcomes.iter());
                span.record("unknown_workplace", rates.unknown_workplace);
                span.record("unknown_salary", rates.unknown_salary);
                span.record("strict_would_drop", rates.strict_would_drop);
                span.record("truncated_results", batch.truncated_results);
                debug!(
                    source = %name,
                    count = batch.outcomes.len(),
                    failed = batch.failed,
                    "source fetched"
                );
                fetches.push(SourceFetch {
                    source: name.clone(),
                    batch: Ok(batch),
                });
            }
            Err(err) => {
                warn!(error = %err, source = %name, "source fetch failed");
                fetches.push(SourceFetch {
                    source: name.clone(),
                    batch: Err(err),
                });
            }
        }
    }
    fetches
}

/// Re-extract a list of posting URLs (the thin-source case) into outcomes,
/// counting per-posting failures (non-fatal). Shared by thin adapters
/// (Remotive): the driver keeps the failure-summary contracts — `Err` is a
/// whole-source failure, `failed` counts per-posting failures.
pub(crate) async fn ingest_postings<F: Fetcher>(
    urls: Vec<Url>,
    client: &PoliteClient<F>,
) -> SourceBatch {
    let mut outcomes = Vec::with_capacity(urls.len());
    let mut failed = 0;
    for url in urls {
        match ingest::ingest_url(&url, client).await {
            Ok(outcome) => outcomes.push(outcome),
            Err(err) => {
                warn!(error = %err, url = %url, "posting ingest failed");
                failed += 1;
            }
        }
    }
    SourceBatch {
        outcomes,
        failed,
        truncated_results: 0,
        discovered_at: None,
    }
}

/// Build the sources to run: every enabled source, or exactly the one named
/// by `--source` (explicit naming is itself the opt-in, so it may name a
/// disabled source; an unknown name is an error). `prior_watermark` carries
/// each source's last-recorded `discovered_at`, passed to paid sources as
/// their `discovered_at_gte` lower bound.
#[instrument(skip_all)]
pub fn build_sources(
    config: &Config,
    only: Option<&str>,
    prior_watermark: &HashMap<String, String>,
) -> Result<Vec<(String, Box<dyn DiscoverySource>)>> {
    let mut sources = Vec::new();
    if let Some(name) = only {
        sources.push((
            name.to_string(),
            make_source(config, name, prior_watermark.get(name).cloned())?,
        ));
    } else {
        let mut enabled: Vec<&String> = config
            .sources
            .iter()
            .filter(|(_, source)| source.enabled)
            .map(|(name, _)| name)
            .collect();
        enabled.sort();
        for name in enabled {
            sources.push((
                name.clone(),
                make_source(config, name, prior_watermark.get(name).cloned())?,
            ));
        }
    }
    Ok(sources)
}

/// The source registry: name → adapter. Extend here as adapters land
/// (WWR, …).
fn make_source(
    config: &Config,
    name: &str,
    watermark: Option<String>,
) -> Result<Box<dyn DiscoverySource>> {
    match name {
        "remotive" => Ok(Box::new(remotive::Remotive::new())),
        "theirstack" => {
            let source = config
                .sources
                .get("theirstack")
                .cloned()
                .unwrap_or_default();
            Ok(Box::new(theirstack::Theirstack::new(
                &source, config, watermark,
            )))
        }
        other => Err(miette!(
            "unknown source '{other}' (known: remotive, theirstack)"
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, sync::Mutex};

    use miette::bail;

    use super::*;
    use crate::{config::SourceConfig, domain::events::ExtractedFields, ingest::FetchResponse};

    fn outcome(url: &str) -> IngestOutcome {
        IngestOutcome {
            adapter: "drop-in".into(),
            source: "unknown".into(),
            url: Some(url.into()),
            raw_text: "body".into(),
            extracted: ExtractedFields::default(),
        }
    }

    // ── build_sources (config → sources) ─────────────────────────

    fn config_with(source: &str, enabled: bool) -> Config {
        let mut config = Config::default();
        config.sources.insert(
            source.to_string(),
            SourceConfig {
                enabled,
                ..Default::default()
            },
        );
        config
    }

    #[test]
    fn build_sources_unknown_name_errors() {
        let config = Config::default();
        assert!(build_sources(&config, Some("bogus"), &HashMap::new()).is_err());
    }

    #[test]
    fn build_sources_runs_disabled_source_when_named() {
        let config = config_with("remotive", false);
        let sources = build_sources(&config, Some("remotive"), &HashMap::new()).unwrap();
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].0, "remotive");
    }

    #[test]
    fn build_sources_empty_when_none_enabled() {
        let config = config_with("remotive", false);
        assert!(
            build_sources(&config, None, &HashMap::new())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn build_sources_enabled_unknown_source_errors() {
        let mut config = config_with("remotive", false);
        config.sources.insert(
            "wwr".to_string(),
            SourceConfig {
                enabled: true,
                ..Default::default()
            },
        );
        // `wwr` is not a known source yet, so building it must error.
        assert!(build_sources(&config, None, &HashMap::new()).is_err());
    }

    #[test]
    fn build_sources_yields_theirstack_adapter() {
        let mut config = Config::default();
        config.sources.insert(
            "theirstack".to_string(),
            SourceConfig {
                enabled: true,
                api_key: "k".into(),
                ..Default::default()
            },
        );
        let sources = build_sources(&config, None, &HashMap::new()).unwrap();
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].0, "theirstack");
        assert!(sources[0].1.charges_per_record());
    }

    // ── fetch_sources (source-level failure) ─────────────────────

    struct FakeSource {
        fail: bool,
        batch: SourceBatch,
    }

    impl DiscoverySource for FakeSource {
        fn fetch<'a>(
            &'a self,
            _client: &'a HttpClient,
        ) -> Pin<Box<dyn Future<Output = Result<SourceBatch>> + Send + 'a>> {
            let fail = self.fail;
            let outcomes = self.batch.outcomes.clone();
            let failed = self.batch.failed;
            Box::pin(async move {
                if fail {
                    bail!("feed down");
                } else {
                    Ok(SourceBatch {
                        outcomes,
                        failed,
                        truncated_results: 0,
                        discovered_at: None,
                    })
                }
            })
        }
    }

    #[tokio::test(start_paused = true)]
    async fn one_failed_source_does_not_block_the_other() {
        let client = crate::ingest::default_client().unwrap();
        let sources: Vec<(String, Box<dyn DiscoverySource>)> = vec![
            (
                "ok".into(),
                Box::new(FakeSource {
                    fail: false,
                    batch: SourceBatch {
                        outcomes: vec![outcome("https://example.com/a")],
                        failed: 0,
                        truncated_results: 0,
                        discovered_at: None,
                    },
                }),
            ),
            (
                "down".into(),
                Box::new(FakeSource {
                    fail: true,
                    batch: SourceBatch {
                        outcomes: vec![],
                        failed: 0,
                        truncated_results: 0,
                        discovered_at: None,
                    },
                }),
            ),
        ];

        let fetches = fetch_sources(&sources, &client).await;

        assert!(fetches[0].batch.is_ok());
        assert!(fetches[1].batch.is_err());
    }

    // ── null_rates ───────────────────────────────────────────────

    #[test]
    fn null_rates_counts_unknown_and_strict() {
        let known_comp = || crate::domain::events::CompRange {
            min: Some(100_000),
            max: None,
            currency: "USD".into(),
            period: "year".into(),
            raw: "$100,000".into(),
        };
        let mut remote_unknown = outcome("https://example.com/a");
        remote_unknown.extracted.remote = None;
        remote_unknown.extracted.comp = Some(known_comp());
        let mut salary_unknown = outcome("https://example.com/b");
        salary_unknown.extracted.remote = Some(true);
        salary_unknown.extracted.comp = None;
        let mut known = outcome("https://example.com/c");
        known.extracted.remote = Some(true);
        known.extracted.comp = Some(known_comp());

        let rates = null_rates(vec![&remote_unknown, &salary_unknown, &known]);

        assert_eq!(rates.unknown_workplace, 1);
        assert_eq!(rates.unknown_salary, 1);
        // One unknown-workplace record + one unknown-salary record.
        assert_eq!(rates.strict_would_drop, 2);
    }

    // ── ingest_postings (posting-level failure) ───────────────────

    struct ScriptedFetcher {
        responses: Mutex<VecDeque<FetchResponse>>,
    }

    impl ScriptedFetcher {
        fn with(responses: Vec<FetchResponse>) -> Self {
            Self {
                responses: Mutex::new(responses.into()),
            }
        }
    }

    impl Fetcher for ScriptedFetcher {
        async fn get(&self, _url: &Url) -> Result<FetchResponse> {
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| miette::miette!("scripted fetcher ran out of responses"))
        }

        async fn post(
            &self,
            _url: &Url,
            _body: String,
            _bearer: Option<String>,
        ) -> Result<FetchResponse> {
            Err(miette::miette!("post not scripted"))
        }
    }

    #[tokio::test(start_paused = true)]
    async fn one_dead_posting_continues_and_counts_failed() {
        let html = "<html><head><title>Engineer — Acme</title></head><body>\
                    <article><h1>Platform Engineer</h1>\
                    <p>Remote role building platforms with a wonderful team of \
                    experienced engineers.</p></article></body></html>";
        let fetcher = ScriptedFetcher::with(vec![
            FetchResponse {
                status: 200,
                retry_after: None,
                final_url: None,
                body: html.into(),
            },
            FetchResponse {
                status: 404,
                retry_after: None,
                final_url: None,
                body: String::new(),
            },
        ]);
        let client = PoliteClient::new(fetcher);
        let urls = vec![
            Url::parse("https://example.com/ok").unwrap(),
            Url::parse("https://example.com/dead").unwrap(),
        ];

        let batch = ingest_postings(urls, &client).await;

        assert_eq!(batch.outcomes.len(), 1);
        assert_eq!(batch.failed, 1);
        assert_eq!(batch.truncated_results, 0);
        assert_eq!(batch.discovered_at, None);
    }
}
