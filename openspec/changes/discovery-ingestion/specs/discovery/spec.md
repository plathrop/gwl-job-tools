## Purpose

Batch discovery of job postings from pluggable feed sources, resolving each
discovered posting to its canonical URL and feeding it through the existing
ingest/dedupe pipeline.

## ADDED Requirements

### Requirement: Discover fetches postings from enabled sources

The `discover` command SHALL fetch postings from every enabled feed source
and ingest each posting through the existing pipeline (fetch → extract →
gate → score → dedupe → persist).

#### Scenario: Discover ingests postings from an enabled source

- **WHEN** the operator runs `discover` with a feed source enabled in config
- **THEN** the command fetches that source's postings and runs each through
  the pipeline

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
leads for unchanged postings.

#### Scenario: Re-running an unchanged feed

- **WHEN** discovery re-runs a source whose postings were all ingested in a
  prior run
- **THEN** no new leads are created; existing leads are matched by dedupe and
  reported as re-ingests (or suppressed)

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
