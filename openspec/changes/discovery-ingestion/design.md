## Context

See `proposal.md` for motivation. This design assumes the existing v0 seams:

- **Transport**: `Fetcher` trait + `PoliteClient` (politeness delay, honors
  `Retry-After`) over `HttpFetcher` (reqwest). `get_text`/`get_json` helpers.
- **Per-posting ingest**: `ingest::ingest_url` — detects a known board from
  the URL host, fetches its public JSON API first, falls back to HTML +
  main-text extraction, and returns an `IngestOutcome` (`adapter`, `source`,
  `url`, `raw_text`, `extracted`).
- **Decide/append core**: `commands::record_ingest` — computes identity,
  matches against the projection, replays the matched stream, evaluates
  gates + scoring, and appends `ingested`/`updated`/`reingest_suppressed`
  (plus `rejected`/`scored`). It is the single entry point discovery reuses.
- **Read model**: `Projection` is rebuilt by folding the log; `lookup`
  resolves an incoming identity to a lead id via `req:` → `url:` → `tc:` →
  dedupe-key precedence.
- **Durability**: single writer via `flock`; a batch `append` is one
  `write(2)` + `fsync` and returns the appended envelopes
  (`EventStore::append -> Vec<EventEnvelope>`).

## Goals / Non-Goals

**Goals:**

- A feed-agnostic discovery seam (`DiscoverySource` trait) plus a batch
  driver that runs postings through the existing ingest pipeline.
- One free adapter (Remotive) wired up as proof of the interface.
- Redirect-aware canonicalization so proxy URLs resolve to their canonical
  ATS posting and earn the same strong dedupe keys as direct URLs.

**Non-Goals (deferred, named for later increments):**

- Fuzzy/Jaccard title dedup — the strong keys (`req:`/`url:`) deliver robust
  dedup at volume; fuzzy is a follow-up if measured residual duplicates
  warrant it.
- Posting pre-filtering on feed metadata (drop obvious non-remote /
  below-floor / blacklisted before fetching). Its only real payoff is paid
  credit control, so it lands with the paid tier.
- Durable "seen but not fetched" memory (a discovery cursor), which the
  pre-filter implies.
- Top-N daily curation, an embedded scheduler, and paid adapters.

## Decisions

### 1. Resolution lives in `ingest_url`, not a second trait

A posting URL that is a proxy (Remotive/WWR) redirects to the real ATS; a
paid feed's URL is already canonical. Both are the *same* downstream problem —
"ingest this URL" — so resolution belongs in the ingest layer, not per
adapter.

- `HttpFetcher::get` must capture `Response::url()` (the post-redirect final
  URL), currently discarded.
- `ingest_url` re-runs `platforms::detect` on the final URL, so a proxy URL
  that lands on Greenhouse/Ashby/Lever/Workday gets the API-first path and
  records the canonical URL (not the proxy) in `IngestOutcome.url`.

A proxy posting therefore costs two fetches — the HTML fetch that resolves
the redirect, then the ATS API fetch after re-detection — and both go
through `PoliteClient`, so each carries the 300ms politeness delay.

*Alternative considered*: a composable `UrlResolver` trait at the adapter
layer. Rejected — resolution is transport knowledge, and folding it into
`ingest_url` also fixes the single-URL `ingest` command for any proxy link
(e.g. `lnkd.in`), which a feed-scoped resolver would not.

### 2. `DiscoverySource` yields postings; the driver fetches the source

```rust
pub struct Posting {
    pub url: Url,
    // Feed-provided hints only — never the authoritative snapshot.
    pub title: Option<String>,
    pub company: Option<String>,
    pub comp: Option<CompRange>,
    pub location: Option<String>,
    pub remote: Option<bool>,
    pub req_id: Option<String>,
}

pub trait DiscoverySource {
    fn postings(&self, client: &HttpClient) -> impl Future<Output = Result<Vec<Posting>>>;
}
```

Native `async fn` in traits (edition 2024) — no `async_trait` dependency.
Posting metadata is a *hint* for a future pre-filter; this increment ignores
it for gating decisions and always re-extracts from the canonical source
(decision 3). The trait is the seam a paid adapter implements later with no
new design.

### 3. Always re-extract from the canonical source

Feed metadata is unreliable and (for free feeds) never carries a `req:` key,
which is the key that collapses cross-host/cross-feed duplicates. The driver
always runs `ingest_url` on the posting URL and discards the feed's field
hints for the snapshot; `source` = feed name, `adapter` = detected ATS.
*Alternative*: trust the feed's description when present — rejected because
it loses the `req:` key and makes extraction quality feed-dependent.

### 4. Batch driver: fetch outside the lock, one lock for the decides

The driver mirrors `execute_ingest`'s discipline (network waits never hold
the writer lock): it fetches + extracts every posting without the lock, then
acquires the lock once and runs the decide/append loop. This preserves the
single-writer durability contract while keeping politeness delays out of the
critical section.

### 5. Projection consistency: re-project between postings

The existing `record_ingest` takes a `&Projection` snapshot and never
refreshes it, so a naive loop over one snapshot would mint duplicate leads
for within-batch duplicates. The driver keeps the read model honest by
re-projecting after each posting: replay the (small, append-only) log and
`Projection::rebuild`, exactly as every existing command does. This is the
event-sourcing-idiomatic approach — the read model is always
`rebuild(log)`, and single-writer locking means there is no concurrency to
reconcile. The cost is `O(n · log)` but negligible at batch scale (the 300ms
politeness delay per fetch dominates).

*Alternative*: incremental projection — fold each appended envelope into the
`Projection` without re-reading. Correct and cheaper, but requires exposing a
per-event fold that today lives inline in `rebuild`. Left as the documented
scaling path (the seam exists: `append` returns the envelopes), not built
now.

### 6. Per-posting and per-source failures are non-fatal; store corruption is fatal

One dead link must not kill a batch run. A posting whose fetch/extract fails
is `warn!`-logged with its URL, counted in the summary as `failed`, and the
loop continues. One level up, the same holds: a source whose fetch fails
(feed down, malformed response, timeout) is logged with the source name, the
run continues with the remaining enabled sources, and the failed source is
reported in the summary. Store/consistency errors (lock failure, malformed
log, projection corruption) abort the whole run — those are source-of-truth
failures, not per-posting or per-source noise.

### 7. Config: opt-in `[sources.<name>]`

```toml
[sources.remotive]
enabled = true
```

Sources default to disabled. `discover` with no flag runs all enabled
sources; `discover --source remotive` runs exactly that source for this
invocation — explicit naming is itself the opt-in, so it may name a disabled
source, and an unknown name is an error. No enabled sources (and no
`--source`) is a clean no-op. Paid sources later add credential/env wiring
under the same table, keeping "never contact a service you haven't
approved" as the default.

## Observability

The operator must be able to answer "did this run find good leads, and how
were they handled?" per the project's observability-by-default contract:

- `#[instrument]` spans: one per source fetch (feed, posting count, outcome)
  and one per posting ingest (feed, canonical URL, kind:
  new/updated/suppressed/rejected/failed).
- `info!` on run start/finish with the summary counts; `debug!` per posting;
  `warn!`/`error!` on fetch/extract failure carrying the URL.
- The summary is also printed as command output (human + `--json`).

## Risks / Trade-offs

- **A feed's proxy URL resolves to a non-ATS page** (e.g. a job that moved) →
  `ingest_url` falls back to HTML and may store a less-clean canonical URL.
  Mitigation: the same path a hand-pasted URL takes; a `failed` count
  surfaces it.
- **Redirect capture changes single-URL ingest behavior** → the canonical
  `url:` dedupe key now reflects the resolved ATS URL, so a previously
  ingested proxy-URL lead re-ingested post-change may surface an `url`
  change in `changed`. Mitigation: expected and desirable (the key is now
  more correct); noted in the PR so reviewers are not surprised.
- **Re-projection per posting is `O(n · log)`** → fine now; incremental fold
  is the scaling path if batch sizes grow.
- **Feed metadata unused in this increment** → the `Posting` fields are dead
  weight until the paid tier's pre-filter. Mitigation: kept minimal and
  documented as the pre-filter seam; no code consumes them yet.

## Migration Plan

Additive change — no event-schema or config migration. Existing leads are
unaffected; `discover` simply starts ingesting under new `source` values.
No rollback needed beyond reverting the commit (the log remains valid).

## Open Questions

None blocking. Two deferrable follow-ups, each named as its own increment:
fuzzy dedup (pending a measured duplicate rate after strong-key dedup lands)
and the paid tier (pending a look at TheirStack/Coresignal pricing and API
shapes, which the feed-agnostic trait is designed to absorb).
