//! Discovery layer: pluggable feed sources that yield job postings for the
//! ingest pipeline (OpenSpec change `discovery-ingestion`).
//!
//! A [`DiscoverySource`] fetches [`Posting`]s — a posting URL plus
//! feed-provided metadata hints. The hints are never the authoritative
//! snapshot: the driver always re-extracts from the canonical source via
//! [`crate::ingest::ingest_url`]. They exist as the seam for a future
//! feed-metadata pre-filter (paid-tier credit control).

pub mod remotive;

use std::{future::Future, pin::Pin};

use miette::{Result, miette};
use tracing::{debug, instrument, warn};
use url::Url;

use crate::{
    config::Config,
    domain::events::CompRange,
    ingest::{self, Fetcher, HttpClient, IngestOutcome, PoliteClient},
};

/// A job posting reference yielded by a discovery source.
#[derive(Clone, Debug)]
pub struct Posting {
    pub url: Url,
    /// Feed-provided hints only — never the authoritative snapshot.
    pub title: Option<String>,
    pub company: Option<String>,
    pub comp: Option<CompRange>,
    pub location: Option<String>,
    pub remote: Option<bool>,
    pub req_id: Option<String>,
}

/// A feed source: fetch its job postings. Feed-agnostic — free curated feeds
/// (Remotive/WWR) now, paid commercial APIs (TheirStack/Coresignal) later.
///
/// The boxed-future return (rather than native `async fn`) keeps the trait
/// dyn-compatible, so sources can be held as `Box<dyn DiscoverySource>`.
pub trait DiscoverySource {
    fn postings<'a>(
        &'a self,
        client: &'a HttpClient,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Posting>>> + Send + 'a>>;
}

/// One source's fetch result; `Err` means the whole source failed (feed
/// down, malformed response, timeout) — non-fatal for the run.
#[derive(Debug)]
pub struct SourceFetch {
    pub source: String,
    pub postings: Result<Vec<Posting>>,
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
        match source.postings(client).await {
            Ok(postings) => {
                debug!(source = %name, count = postings.len(), "source fetched");
                fetches.push(SourceFetch {
                    source: name.clone(),
                    postings: Ok(postings),
                });
            }
            Err(err) => {
                warn!(error = %err, source = %name, "source fetch failed");
                fetches.push(SourceFetch {
                    source: name.clone(),
                    postings: Err(err),
                });
            }
        }
    }
    fetches
}

/// Fetch + extract each posting (no writer lock; network waits never hold
/// the lock). A posting whose fetch/extract fails is logged and counted, not
/// fatal (spec: "A failed posting does not abort the run"). Returns the
/// extracted `(source, outcome)` pairs and the failed count.
#[instrument(skip_all)]
pub async fn fetch_postings<F: Fetcher>(
    postings: Vec<(String, Posting)>,
    client: &PoliteClient<F>,
) -> (Vec<(String, IngestOutcome)>, u64) {
    let mut outcomes = Vec::with_capacity(postings.len());
    let mut failed = 0;
    for (source, posting) in postings {
        match ingest::ingest_url(&posting.url, client).await {
            Ok(outcome) => outcomes.push((source, outcome)),
            Err(err) => {
                warn!(error = %err, url = %posting.url, source, "posting ingest failed");
                failed += 1;
            }
        }
    }
    (outcomes, failed)
}

/// Build the sources to run: every enabled source, or exactly the one named
/// by `--source` (explicit naming is itself the opt-in, so it may name a
/// disabled source; an unknown name is an error).
#[instrument(skip_all)]
pub fn build_sources(
    config: &Config,
    only: Option<&str>,
) -> Result<Vec<(String, Box<dyn DiscoverySource>)>> {
    let mut sources = Vec::new();
    if let Some(name) = only {
        sources.push((name.to_string(), make_source(name)?));
    } else {
        let mut enabled: Vec<&String> = config
            .sources
            .iter()
            .filter(|(_, source)| source.enabled)
            .map(|(name, _)| name)
            .collect();
        enabled.sort();
        for name in enabled {
            sources.push((name.clone(), make_source(name)?));
        }
    }
    Ok(sources)
}

/// The source registry: name → adapter. Extend here as adapters land
/// (WWR, TheirStack, Coresignal, …).
fn make_source(name: &str) -> Result<Box<dyn DiscoverySource>> {
    match name {
        "remotive" => Ok(Box::new(remotive::Remotive::new())),
        other => Err(miette!("unknown source '{other}' (known: remotive)")),
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, sync::Mutex};

    use miette::bail;

    use super::*;
    use crate::ingest::FetchResponse;

    fn posting(url: &str) -> Posting {
        Posting {
            url: Url::parse(url).unwrap(),
            title: None,
            company: None,
            comp: None,
            location: None,
            remote: None,
            req_id: None,
        }
    }

    // ── build_sources (config → sources) ─────────────────────────

    fn config_with(source: &str, enabled: bool) -> Config {
        let mut config = Config::default();
        config
            .sources
            .insert(source.to_string(), crate::config::SourceConfig { enabled });
        config
    }

    #[test]
    fn build_sources_unknown_name_errors() {
        let config = Config::default();
        assert!(build_sources(&config, Some("bogus")).is_err());
    }

    #[test]
    fn build_sources_runs_disabled_source_when_named() {
        let config = config_with("remotive", false);
        let sources = build_sources(&config, Some("remotive")).unwrap();
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].0, "remotive");
    }

    #[test]
    fn build_sources_empty_when_none_enabled() {
        let config = config_with("remotive", false);
        assert!(build_sources(&config, None).unwrap().is_empty());
    }

    #[test]
    fn build_sources_runs_only_enabled() {
        let mut config = config_with("remotive", false);
        config.sources.insert(
            "wwr".to_string(),
            crate::config::SourceConfig { enabled: true },
        );
        // `wwr` is not a known source yet, so building it must error.
        assert!(build_sources(&config, None).is_err());
    }

    // ── fetch_sources (source-level failure) ─────────────────────

    struct FakeSource {
        fail: bool,
        postings: Vec<Posting>,
    }

    impl DiscoverySource for FakeSource {
        fn postings<'a>(
            &'a self,
            _client: &'a HttpClient,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<Posting>>> + Send + 'a>> {
            let fail = self.fail;
            let postings = self.postings.clone();
            Box::pin(async move {
                if fail {
                    bail!("feed down")
                } else {
                    Ok(postings)
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
                    postings: vec![posting("https://example.com/a")],
                }),
            ),
            (
                "down".into(),
                Box::new(FakeSource {
                    fail: true,
                    postings: vec![],
                }),
            ),
        ];

        let fetches = fetch_sources(&sources, &client).await;

        assert!(fetches[0].postings.is_ok());
        assert!(fetches[1].postings.is_err());
    }

    // ── fetch_postings (posting-level failure) ───────────────────

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
        let postings = vec![
            ("remotive".to_string(), posting("https://example.com/ok")),
            ("remotive".to_string(), posting("https://example.com/dead")),
        ];

        let (outcomes, failed) = fetch_postings(postings, &client).await;

        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].0, "remotive");
        assert_eq!(failed, 1);
    }
}
