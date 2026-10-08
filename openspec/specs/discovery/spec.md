# discovery Specification

## Purpose

Batch discovery of job postings from pluggable feed sources, resolving each
discovered posting to its canonical URL and feeding it through the existing
ingest/dedupe pipeline.

## Requirements

### Requirement: Discover fetches postings from enabled sources

The `discover` command SHALL fetch postings from every enabled feed source
and ingest each posting through the pipeline (fetch → extract → gate → score
→ dedupe → persist). A source MAY supply already-extracted postings; in that
case the driver SHALL ingest the supplied snapshot directly rather than
fetching and extracting the posting again.

#### Scenario: Discover ingests postings from an enabled source

- **WHEN** the operator runs `discover` with a feed source enabled in config
- **THEN** the command fetches that source's postings and runs each through
  the pipeline

#### Scenario: A source supplies already-extracted postings

- **WHEN** an enabled source yields a posting whose text and fields it has
  already extracted
- **THEN** the driver ingests that snapshot directly, without a second fetch

#### Scenario: No enabled sources is a clean no-op

- **WHEN** the operator runs `discover` with no feed sources enabled
- **THEN** the command reports nothing to do and does not modify the event log

### Requirement: Discovered URLs resolve to canonical posting URLs

A discovered posting URL that is a proxy or redirect SHALL be resolved to
its final canonical posting URL before ingestion, and known ATS platforms
SHALL be detected from the resolved URL, so proxy URLs receive the same
extraction and dedupe treatment as directly-pasted posting URLs.

#### Scenario: Proxy URL resolves to a known board

- **WHEN** a discovered posting URL redirects to a known board's posting
- **THEN** the posting is ingested via that board's API-first extraction, and
  the canonical resolved URL — not the proxy URL — is recorded as the lead's
  URL

### Requirement: Discovered leads carry source and adapter independently

A lead ingested via discovery SHALL record the feed that found it as its
`source` and the extraction platform as its `adapter`, so the two facts are
distinguished.

#### Scenario: Feed source and ATS adapter are distinct

- **WHEN** a posting from a feed is ingested from a known-board posting
- **THEN** the recorded snapshot has `source` equal to the feed name and
  `adapter` equal to the detected board

### Requirement: Discovery never mints duplicate leads

When two postings in one run denote the same job, the second SHALL be
recorded as a re-ingest of the existing lead (`updated` or suppressed),
never as a new lead.

#### Scenario: Same job reached by two postings in one run

- **WHEN** one run's postings include two that share a canonical URL or req
  id
- **THEN** exactly one lead stream is created and the second posting is
  recorded as a re-ingest against that lead

### Requirement: Re-running discovery is idempotent

Re-running discovery over postings already ingested SHALL NOT create new
leads for unchanged postings. For a paid source, a re-run SHALL fetch only
jobs the source discovered after a run that recorded a `discovered_at`
watermark, so already-seen jobs are not re-fetched at cost.

#### Scenario: Re-running an unchanged feed

- **WHEN** discovery re-runs a source whose postings were all ingested in a
  prior run
- **THEN** no new leads are created; existing leads are matched by dedupe and
  reported as re-ingests (or suppressed)

#### Scenario: Re-running a paid source fetches only new jobs

- **WHEN** discovery re-runs a paid source whose prior run recorded a
  `discovered_at` watermark
- **THEN** the source query carries that watermark as its lower bound, and
  jobs the source discovered before it are not fetched again

### Requirement: Durable-ignore is respected during discovery

A posting that matches a durably ignored lead SHALL be suppressed, not
re-added to the review queue.

#### Scenario: Ignored lead re-seen by a feed

- **WHEN** a posting matches a lead marked `ignore`
- **THEN** the posting is recorded as suppressed and does not re-enter the
  review queue

### Requirement: A failed posting does not abort the run

When a single posting's fetch or extraction fails, the `discover` command
SHALL count it as `failed` and continue with the remaining postings. Store
or event-log consistency failures SHALL abort the run.

#### Scenario: One posting fails among healthy ones

- **WHEN** one posting's fetch or extraction fails while other postings
  ingest normally
- **THEN** the run continues, counts the posting as `failed`, and reports it
  in the summary

#### Scenario: Store or event-log corruption aborts the run

- **WHEN** a store or event-log consistency failure occurs during a run
- **THEN** the run aborts rather than continuing with partial state

### Requirement: A failed source does not abort the run

When one enabled source's fetch fails (feed down, malformed response,
timeout), the `discover` command SHALL report the source failure and
continue fetching the remaining enabled sources.

#### Scenario: One source fails, others run

- **WHEN** one enabled source's fetch fails while another source is enabled
- **THEN** the run continues with the remaining sources and reports the
  failed source in the output or summary

### Requirement: Discovery reports a batch summary

The `discover` command SHALL report the outcome of the run: how many
postings were new, updated, suppressed, rejected, and failed to ingest.

#### Scenario: Run summary

- **WHEN** a discovery run completes
- **THEN** the command outputs a summary carrying new, updated, suppressed,
  rejected, and failed counts

### Requirement: Discovery records a run event

A completed discovery run SHALL append a `discovery` event to the event
log — on a stream distinct from lead streams — carrying the run's summary
(new / updated / suppressed / rejected / failed, and any failed sources).

#### Scenario: A run appends a discovery event

- **WHEN** a discovery run completes
- **THEN** a `discovery` event carrying the run summary is appended to the
  event log

### Requirement: Discover ingests from the TheirStack Jobs API

When the `theirstack` source is enabled and its API key is present, the
`discover` command SHALL fetch jobs from the TheirStack Jobs API and ingest
each returned job through the pipeline. A missing API key SHALL fail the
source — reported and the run continues — rather than silently skipping it.

#### Scenario: TheirStack source ingests jobs

- **WHEN** the operator runs `discover` with `theirstack` enabled and
  `THEIRSTACK_API_KEY` set
- **THEN** the command fetches jobs from the TheirStack Jobs API and ingests
  each through the pipeline

#### Scenario: TheirStack source without a key fails

- **WHEN** the operator runs `discover` with `theirstack` enabled but no
  `THEIRSTACK_API_KEY`
- **THEN** the source is reported as failed and the run continues with the
  remaining sources

### Requirement: Discovery pre-filters blacklisted companies server-side

The blacklist gate SHALL remain in the client-side pipeline — the
authoritative "never match" backstop, unchanged and applied to every fetched
posting. When a source can exclude companies server-side, the `discover`
command SHALL additionally push the configured blacklist into the source
query, so blacklisted companies are never fetched — and, for a paid source,
never charged — as a credit-saving optimization, not a replacement for the
gate.

#### Scenario: Blacklisted companies are not fetched when the source can exclude them

- **WHEN** a paid source is run with a configured blacklist and the source
  supports company exclusion
- **THEN** blacklisted companies' jobs are excluded from the fetch and are
  neither ingested nor charged

#### Scenario: A blacklisted company that evades the source filter is still rejected

- **WHEN** a source's server-side exclusion misses a blacklisted company, or
  the source has no exclusion filter
- **THEN** the fetched job is rejected by the client-side blacklist gate and
  recorded as rejected

### Requirement: Permissive gates are client-side unless opted into strict mode

By default, the `discover` command SHALL apply the remote-only and
compensation-floor gates client-side to fetched postings, so unknown-status
postings reach review. A source that supports server-side filtering of those
gates SHALL have it disabled by default and SHALL document what its strict
mode excludes.

#### Scenario: Unknown-status postings reach review by default

- **WHEN** a paid source is run without strict mode enabled
- **THEN** remote-only and compensation-floor are applied to the fetched
  postings, and postings whose status is unknown are not dropped

#### Scenario: Strict mode excludes what it documents

- **WHEN** the operator enables a source's strict mode
- **THEN** the source query excludes the postings its documented strict mode
  covers, and the rest flow through the client-side gates

### Requirement: Discovery reports credit exhaustion

When a paid source returns fewer results than exist because the account ran
out of credits, the `discover` command SHALL report the number of truncated
(unreturned) results in its summary. Credit exhaustion may surface either as
a truncation marker in a successful response or as a payment-required (402)
response; both SHALL be reported as truncated results. A payment-required
response's stated requirement is a lower bound on the unreturned tail (it is
the rejected page's own requirement, not the whole tail), and the reported
count SHALL reflect at least that bound.

#### Scenario: Credit exhaustion is reported

- **WHEN** a paid source's response indicates it could not return some results
  because credits ran out
- **THEN** the run summary reports the truncated-result count

#### Scenario: A payment-required response is reported as truncation

- **WHEN** a paid source rejects a page request because the remaining credit
  balance cannot cover it (HTTP 402), and the response states how many
  credits the rejected results required
- **THEN** the run summary reports at least that many unreturned results in
  the truncated-result count, and the source's failure reason records the
  payment-required error

#### Scenario: Exhaustion on the first page is reported, not discarded

- **WHEN** a paid source's FIRST page request is rejected as payment-required
- **THEN** no records are ingested for that source, the source is reported as
  failed with the payment-required reason, the unreturned count is reported
  in the truncated-result count, and the run continues

### Requirement: A partial paid fetch keeps its already-fetched records

When a paid source's fetch fails after it has already returned records
(mid-pagination failure, credit exhaustion at a page boundary), the
`discover` command SHALL ingest the already-fetched records, record their
watermark, and report the source as failed with the reason — rather than
discarding the fetched records. A payment-required response keeps this
treatment at ANY page, including the first (exhaustion, not a generic
failure). Any other failure before any record is fetched remains a
whole-source failure.

#### Scenario: Mid-pagination failure salvages the fetched records

- **WHEN** a paid source returns one or more pages of records and a later
  page request fails
- **THEN** the already-fetched records are ingested through the pipeline, the
  run records their watermark, and the source appears in the failed-sources
  list

#### Scenario: A failure before any record is fetched is a whole-source failure

- **WHEN** a paid source's first page request fails for a non-payment reason
  (feed down, malformed response, timeout)
- **THEN** no records are ingested for that source, the source is reported as
  failed, and the run continues with the remaining sources

### Requirement: Source failures carry their reason

The `discover` command SHALL record why each failed source failed — in the
run summary and on the `discovery` run event — so the failure is diagnosable
from the run's own artifacts without re-running at a higher log level, and
SHALL log source fetch failures at the error level.

#### Scenario: The failure reason is recorded

- **WHEN** a source's fetch fails or ends early, in whole or in part
- **THEN** the run summary and the `discovery` event carry the reason keyed by
  source, and the log records the failure at the error level

### Requirement: A paid source's query is operator-shapeable

When a paid source's config declares a `query` table, the `discover` command
SHALL merge every key of that table into the source's search request body,
so credits are spent only on postings the operator has chosen to pay for.
Keys the adapter itself generates — pagination, recency, the watermark,
blacklist pre-filters, and strict-mode gates — SHALL be rejected at config
load with an error naming the reserved key. The default query SHALL be
unchanged when no table is declared.

#### Scenario: Declared query filters shape the request

- **WHEN** the operator declares `[sources.theirstack.query]` with one or
  more filter parameters the source's API documents
- **THEN** the search request carries those parameters alongside the
  adapter-generated ones, and the fetched postings still pass through every
  client-side gate

#### Scenario: Reserved keys are rejected

- **WHEN** the operator's `query` table contains a key the adapter generates
  itself (e.g. `limit`, `page`, `posted_at_max_age_days`, `discovered_at_gte`)
- **THEN** the command fails at config load with an error naming the
  reserved key, and no credits are spent

#### Scenario: No query table means the default query

- **WHEN** the operator declares no `query` table
- **THEN** the search request carries only the adapter-generated parameters,
  exactly as before

### Requirement: Paid fetches are bounded by the credit balance

A paid source's fetch SHALL consult the source's credit balance before each
page request and cap the page size to the credits the balance can pay for,
so a small balance yields a small fetch rather than a rejected one.
Multi-page fetches SHALL continue by an explicit offset (the records
already fetched), so a varying page cap is correct by construction: each
request's window starts where the previous page ended, duplicating and
skipping nothing. When the balance cannot pay for any further records, the
fetch SHALL stop as a partial fetch: records already paid for are ingested,
and the reason is carried per the source-failure-reason requirement —
except that once records have been fetched, a zero balance is settled by the
source itself: the fetch issues one further offset request (free under
per-record pricing), and a payment-required (402) response reports the
shortfall as truncated results with the partial-fetch reason, while an
empty response completes the fetch cleanly with no failure. A zero balance
before any record is fetched remains a whole-source failure. A
balance-check failure SHALL NOT block the fetch — the payment-required
response and the partial-fetch salvage are the backstop.

#### Scenario: A small balance yields a bounded fetch

- **WHEN** a paid source is fetched and the account's remaining credits are
  fewer than the source's page size
- **THEN** the page request's size is capped to the remaining credits, the
  records those remaining credits pay for are fetched and ingested, and the
  fetch reports that it stopped early because the balance ran out

#### Scenario: Multi-page continuation duplicates and skips nothing

- **WHEN** a paid fetch spans multiple pages and the page cap varies
  between them (balance or per-run budget smaller than the result set)
- **THEN** every page request continues from the records already fetched,
  and the ingested records are exactly a contiguous prefix of the source's
  result set — none fetched twice, none silently skipped

#### Scenario: A mid-run zero balance is settled by the source

- **WHEN** records have already been fetched and the balance check reads
  zero remaining
- **THEN** the fetch issues one further offset request: if the source
  rejects it as payment-required, the run reports the unreturned records
  as truncated results and the source as failed with the reason; if the
  source returns an empty page, the fetch is complete and no failure is
  reported

#### Scenario: A zero balance stops the fetch before it spends

- **WHEN** a paid source is fetched and the account has no credits remaining
- **THEN** no records are fetched for that source, the source is reported as
  failed with the reason, and the run continues

#### Scenario: A balance-check failure does not block the fetch

- **WHEN** the balance endpoint cannot be reached
- **THEN** the fetch proceeds uncapped and reports a warning, and a
  payment-required response mid-run is handled by the partial-fetch salvage

### Requirement: A per-run credit budget bounds spend

A paid source's config MAY declare a maximum number of credits one run may
spend on that source. The fetch SHALL stop as a partial fetch once the run
has spent the budget — records already paid for are ingested, the reason is
carried — and no further page requests are made.

#### Scenario: The budget stops the run at the declared bound

- **WHEN** a paid source with `max_credits_per_run` set is fetched, and the
  records returned reach the budget
- **THEN** the fetch stops, already-paid records are ingested, and the reason
  records that the budget was reached

#### Scenario: No budget means the balance is the only bound

- **WHEN** a paid source is fetched with no budget declared
- **THEN** the run is bounded only by the account's credit balance

### Requirement: The run summary reports per-source credit spend

For each paid source fetched, the `discover` command SHALL report the number
of credits the run spent on that source (the records it returned), in the run
summary.

#### Scenario: Spend is reported per paid source

- **WHEN** a discovery run fetches a paid source that returns records
- **THEN** the run summary reports the credits spent on that source
