# Spec Delta

## MODIFIED Requirements

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

## ADDED Requirements

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
