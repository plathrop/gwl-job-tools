# Design

## Context

The discovery layer (OpenSpec change `discovery-ingestion`) ships a
`DiscoverySource` trait that yields `Posting { url, hints }`, plus a driver
that fetches each URL via `ingest_url` (fetch → extract) and runs
`record_ingest` under the single-writer lock, re-projecting per posting.
Remotive is the only adapter: proxy URLs + metadata hints that the driver
always re-extracts (discovery design decision 3).

TheirStack's Jobs API (`POST /v1/jobs/search`, decision 0012) is a paid
source with a different shape:

- 1 API credit per record returned; `metadata.truncated_results` signals
  credit exhaustion; 4 req/sec (paid); `limit` up to 500.
- Each record carries the full `description` (markdown) plus structured
  fields (title, company name/domain, `min`/`max`/`avg_annual_salary_usd`,
  `remote`/`hybrid`, `technology_slugs`, `seniority`, `final_url`) — but no
  `req_id`.
- Auth is `Authorization: Bearer`, key from env.
- TheirStack's own periodic-fetch guide recommends a `discovered_at_gte`
  watermark, and names webhooks as the real-time path (deferred,
  GWLJ-8j4zsx).

The trait and driver were built for the "thin" (URL) model; TheirStack is a
"rich" source. This change makes the seam accommodate both, adds the
TheirStack adapter, and adds credit budgeting.

## Goals / Non-Goals

**Goals:**

- One discovery seam that supports both thin (URL → re-extract) and rich
  (already-extracted) sources.
- A TheirStack adapter with correct credit budgeting: never re-fetch a seen
  job, never charge for a blacklisted company, and report credit exhaustion.
- Trust-but-verify: TheirStack fields authoritative, our heuristics run as a
  logged cross-check.

**Non-Goals (deferred):**

- Webhooks (GWLJ-8j4zsx) — the real-time path; needs a listener.
- Fuzzy dedup (GWLJ-n9qnwx) — trigger is measured residual duplicates.
- Pushing remote-only / comp-floor gates server-side *by default* (Decision
  3 — they stay client-side unless `strict_filtering` is opted in).
- Target-companies as a server-side filter (config is empty; it is not a
  gate). Surfacing the credit-balance endpoint is likewise deferred —
  `truncated_results` is the hard requirement.

## Decisions

### 1. The seam: `fetch() -> Result<SourceBatch>`; the adapter owns re-extract vs direct

`DiscoverySource::postings() -> Result<Vec<Posting>>` becomes:

```rust
struct SourceBatch {
    outcomes: Vec<IngestOutcome>,
    failed: u64,
}

trait DiscoverySource {
    fn fetch<'a>(&'a self, client: &'a HttpClient)
        -> Pin<Box<dyn Future<Output = Result<SourceBatch>> + Send + 'a>>;
}
```

- Remotive's adapter now calls `ingest_url` per posting internally (it is
  the re-extract case).
- TheirStack's adapter maps each record to an `IngestOutcome` directly
  (`adapter`/`source` = `theirstack`, `url` = `final_url`, `raw_text` =
  `description`, `extracted` = structured fields).
- The `Posting` hints struct dissolves — its client-side pre-filter role is
  obsolete now that the paid-tier filters are server-side.
- The driver keeps the failure-summary contracts: `Err` = whole source failed
  (non-fatal), `failed` = per-posting failures (non-fatal), store corruption
  aborts the run.

*Alternative*: a `Posting` enum (`Url | Snapshot`) with the driver branching.
Rejected — it forces the driver to know about two shapes; the adapter is the
right place for "do I re-extract or not."

### 2. Trust-but-verify extraction

TheirStack's structured fields are authoritative. The adapter still runs our
`extract_fields` over `description` and compares, emitting span fields (and a
`debug!`) per mismatch — e.g. `comp_structured` vs `comp_extracted`,
`remote_structured` vs `remote_extracted` — keyed by lead id and source.
Never blocking: it exists to make TheirStack data problems visible.

### 3. Blacklist is client-side always, with a server-side pre-filter; permissive gates are opt-in strict

The permissive gates are remote-only and comp-floor, and the spec is explicit
that *unknown passes* ("a false rejection is an invisible loss"). TheirStack's
server filters are strict — a filter on a field excludes records where the
field is absent:

- `workplace_types_or: ["remote"]` would drop unknown-workplace jobs — a
  false rejection the client gate exists to avoid.
- `min_salary_usd` would drop unknown-salary jobs — again a false rejection.
- Skills (`job_technology_slug_or`) is a score dimension, not a gate —
  filtering the feed on it would drop low-scoring jobs that still pass gates.

The blacklist gate stays in the client-side pipeline — unchanged and
authoritative — so "never match blacklisted companies" holds for every source
and the `ingest` path regardless of server-side behavior. Where a source can
exclude companies, the blacklist is *additionally* pushed server-side
(`company_name_not` / `company_domain_not`) as a credit-saving pre-filter:
excluding a blacklisted company from the fetch is cheaper than fetching →
rejecting, but the gate is the backstop, not the filter. Remote-only and
comp-floor stay client-side on the fetched records.

The strict-vs-client-side question for the permissive gates is left to the
data, not settled here: a per-source `strict_filtering` toggle (default off)
lets an adapter push those gates into its query — accepting the
false-rejection risk — and **each adapter documents what its strict mode
excludes**. TheirStack's strict mode maps `remote_only` →
`workplace_types_or: ["remote"]` and `compensation_floor` → `min_salary_usd`.
The default stays client-side until the null-rate telemetry (see
Observability) shows the dropped population is negligible.

### 4. Credit control = watermark + blacklist + limit + recency

- **Watermark**: store per-source `MAX(discovered_at)` and pass it as
  `discovered_at_gte` next run. Chosen over `job_id_not` (passing already-seen
  ids) and over re-fetching everything (re-spends credits).
- **Recency**: TheirStack requires one of `posted_at_*`/company filters.
  Config `[sources.theirstack].posted_at_max_age_days` (default 30) bounds the
  first-run backfill; subsequent runs use the watermark instead.
- **Limit**: page size 500 (TheirStack's max), serial pagination — the 300ms
  politeness delay already sits under the 4 req/sec ceiling.
- `include_total_results` stays off (slow); `metadata.truncated_results` is
  read from the response.

### 5. The watermark lives on the discovery run event

Extend the `discovery` run-event payload with a per-source `discovered_at`
watermark (additive, optional field). The read model derives "last watermark
per source" from the most recent run event for that source. Event-sourced —
the log stays the single source of truth — and additive, so no upcast.
*Alternative*: a sidecar cursor file — rejected, breaks the "JSONL is the
source of truth" invariant.

### 6. Key handling and config

- The source's config block carries the credential as a **reference, not a
  secret**: `[sources.theirstack]` with `api_key = "${THEIRSTACK_API_KEY}"`.
  The config loader resolves `${VAR}` references from the environment at load
  (a step alongside the existing tilde expansion), so the secret stays in env
  and the config file is safe to commit. A missing env var resolves to empty;
  the adapter treats an empty key as "no key" → the source is reported failed
  (run continues), never a silent skip or a hard abort of other sources.
- Config: `[sources.theirstack] enabled = true` plus optional
  `posted_at_max_age_days` and `strict_filtering` on `SourceConfig` (ignored
  by Remotive). The "never contact a service you haven't approved" guardrail
  holds: enabled + a non-empty key is the approval.

## Risks / Trade-offs

- **Cross-feed dedup without `req_id`** → TheirStack carries no req id; dedup
  relies on `url:` (`final_url` canonicalized) and `tc:`. Cross-feed collapse
  with Remotive depends on both resolving to the same canonical URL. Accept
  duplicates for now; fuzzy dedup (GWLJ-n9qnwx) is gated on measured
  residuals.
- **`final_url` vs our canonicalization** → TheirStack's `final_url` may
  carry tracking params; we still run `canonicalize_url`, so it lands on the
  same `url:` key a proxy-resolved Remotive posting would.
- **Watermark boundary re-fetch** → `discovered_at_gte` is inclusive, so the
  boundary job may be re-fetched once; corpus dedup suppresses the re-ingest
  (safe, one credit).
- **Re-projection cost at batch scale** → the driver re-projects per posting
  (O(M·N)); TheirStack's 500/page makes M large for the first time.
  Incremental fold (or SQLite, GWLJ-oujjs3) is the scaling path — measured,
  not bundled here.
- **Trust-but-verify divergence** → persistent disagreement surfaces in the
  mismatch spans without blocking; a future decision can flip authority.

## Migration Plan

Additive: no event-schema break (additive watermark field), no config
migration for existing sources (Remotive ignores the new field). Rollback =
revert the commit; the log stays valid. The `DiscoverySource`/`Posting`
library-surface change is breaking, but v0.1-internal and pre-1.0.

## Observability

Per decision 0011: a span per source fetch (feed, query filters, record
count, truncation) and per posting ingest (canonical URL, kind); `info!` run
start/finish with the summary + credit-exhaustion count; `warn!`/`error!`
with the offending URL/source. Trust-but-verify mismatch spans/logs are the
new failure-mode surface.

The strict-vs-client-side decision is answered by data, so two streams must
land in Honeycomb:

- **Source null rates**: the source-fetch span and run summary record how
  many returned records had unknown workplace type and unknown salary, how
  many the client gates rejected, and how many strict mode would have
  dropped — so the cost/benefit of going strict is directly measurable.
- **Review behavior**: the ingest span carries `remote_known`/`comp_known`,
  and the mark/review spans carry the mark plus the lead's ingest-time
  status, so a Honeycomb query can answer "does the operator actually pursue
  unknown-status leads, and at what rate?" (The event log already holds the
  facts; the spans make them explorable.)
