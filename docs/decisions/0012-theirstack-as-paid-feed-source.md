# 0012: TheirStack as the paid feed source

Status: accepted 2026-09-25 (research + discussion; supersedes the
"TheirStack, Coresignal" framing of GWLJ-71ofy8)

## Context

The discovery layer (OpenSpec change `discovery-ingestion`) shipped the free
Remotive adapter behind the `DiscoverySource` trait and named the paid tier
as "paid commercial APIs (TheirStack, Coresignal) later." The paid tier
(GWLJ-71ofy8) was scoped as "TheirStack, Coresignal" pending a look at
pricing, auth, and API shapes. This record settles which commercial feed to
integrate.

## Options

- **TheirStack Jobs API** (`POST /v1/jobs/search`) — chosen.
- **Coresignal** job-postings dataset.

## Decision

The paid feed tier integrates **TheirStack's Jobs API** as the first paid
`DiscoverySource`. Coresignal is out of scope for now — it may be revisited
only if a data-warehouse/history use case emerges, since it is not a
job-search tool.

Companion decision taken at the same time: the paid integration ships in the
**default build** — no crate-feature gating. Config `enabled` plus a present
API key is the approval gate, the same mechanism that already backs "never
contact a service you haven't approved."

## Rationale

- **Shaped for the job search, not for a data warehouse.** TheirStack's Jobs
  API filters by job title, technologies, seniority, salary, workplace type
  (remote/hybrid/on-site), country, and posting date — the exact dimensions
  the gate → score pipeline consumes. Coresignal focuses on complex querying
  of history and bulk data dumps; its API is significantly less approachable
  for this use.
- **Returns the posting, not just a pointer.** Each job record carries the
  full `description` (markdown) plus structured salary, remote/hybrid,
  company, and technology slugs — so a lead can be ingested without a second
  fetch of the canonical page.
- **Credit model is legible and monitorable.** One API credit per record
  returned; `metadata.truncated_results` reports credit exhaustion. Account
  endpoints (`GET /v0/billing/credit-balance`,
  `GET /v0/teams/credits_consumption`, `GET /v0/requests`) make the
  credit-budgeting requirement observable rather than guessed.
- **Well-designed API surface.** REST + JSON, `Authorization: Bearer` auth,
  a published OpenAPI spec, and a 4 req/sec paid rate limit that the
  existing politeness layer already respects (300 ms delay ≈ 3.3 req/sec).

## Consequences

- GWLJ-71ofy8 is rescoped from "TheirStack, Coresignal" to "TheirStack paid
  feed adapter."
- The paid tier reopens two discovery-layer design decisions, to be settled
  in the change's `design.md`:
  - **Re-extraction** (discovery design decision 3, "always re-extract from
    the canonical source") is revisited: a TheirStack record already carries
    the JD text and structured fields, so re-fetching the canonical URL is
    redundant and slow. A snapshot ingest path is warranted for rich
    sources.
  - **Credit budgeting** (GWLJ-0uqqlm) becomes (a) server-side query
    filtering — push remote/comp/tech/title filters into the query so
    credits are only spent on gate-passing jobs — and (b) a
    `discovered_at_gte` watermark cursor so periodic re-runs fetch only
    newly-discovered jobs.
- Coresignal is not integrated; a future record would be needed to revisit
  it.
