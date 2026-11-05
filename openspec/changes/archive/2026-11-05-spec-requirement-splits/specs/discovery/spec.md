# Spec Delta

## MODIFIED Requirements

### Requirement: Discovery reports credit exhaustion

When a paid source returns fewer results than exist because the account ran
out of credits, the `discover` command SHALL report the number of truncated
(unreturned) results in its summary. Credit exhaustion may surface either as
a truncation marker in a successful response or as a payment-required (402)
response; both SHALL be reported as truncated results.

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
discarding the fetched records. Any other failure before any record is
fetched remains a whole-source failure.

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

### Requirement: Paid fetches are bounded by the credit balance

A paid source's fetch SHALL consult the source's credit balance before each
page request and cap the page size to the credits the balance can pay for,
so a small balance yields a small fetch rather than a rejected one.

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

## ADDED Requirements

### Requirement: A payment-required shortfall is a lower bound

A payment-required response's stated requirement is a lower bound on the
unreturned tail — it is the rejected page's own requirement, not the whole
tail — and the reported count SHALL reflect at least that bound.

#### Scenario: The shortfall is never reported as an exact total

- **WHEN** a payment-required response states the credits its rejected page
  required, while more results may exist beyond that page
- **THEN** the truncated-result count is at least the stated requirement, and
  is not presented as the exact size of the unreturned tail

### Requirement: A payment-required response is a partial fetch at any page

A payment-required response SHALL keep the partial-fetch treatment at ANY
page, including the first: credit exhaustion at a page boundary is not a
generic source failure.

#### Scenario: A first-page rejection is exhaustion, not a generic failure

- **WHEN** a paid source's first page request is rejected as payment-required
- **THEN** the source is treated as exhausted rather than generically failed:
  no records are ingested, the reason is payment-required, and the run
  continues

### Requirement: Multi-page paid fetches continue by explicit offset

Multi-page paid fetches SHALL continue by an explicit offset (the records
already fetched), so a varying page cap is correct by construction: each
request's window starts where the previous page ended, duplicating and
skipping nothing.

#### Scenario: A varying cap still yields a contiguous prefix

- **WHEN** a paid fetch's page cap differs from one page to the next, because
  the credit balance or the per-run budget is smaller than the result set
- **THEN** each request's window starts after the records already fetched, and
  the ingested records contain no duplicate and no gap

### Requirement: Credit exhaustion mid-fetch is settled at the source

When the balance cannot pay for any further records, the fetch SHALL stop as
a partial fetch: already-paid records are ingested, and the reason is
carried per the source-failure-reason requirement. Once records have been
fetched, a zero balance is settled by the source: the fetch issues one
further offset request (free under per-record pricing). A payment-required
(402) response reports the shortfall as truncated results with the
partial-fetch reason; an empty response completes cleanly.

#### Scenario: A drained balance is confirmed by the source

- **WHEN** records have already been fetched and the balance check reads zero
  remaining
- **THEN** the fetch makes one further offset request, and its outcome decides
  the run: a payment-required rejection is reported as truncated results with
  the partial-fetch reason, an empty page as a clean completion

### Requirement: A zero balance before any record is fetched fails the source

A zero balance before any record is fetched SHALL remain a whole-source
failure: no records are fetched for that source, the source is reported as
failed with the reason, and the run continues with the remaining sources.

#### Scenario: A pre-fetch zero balance spends nothing

- **WHEN** a paid source is fetched and the account has no credits remaining
- **THEN** no page request is made, no records are ingested, and the source
  appears in the failed-sources list with the reason

### Requirement: A balance-check failure does not block the fetch

A balance-check failure SHALL NOT block the fetch — the payment-required
response and the partial-fetch salvage are the backstop.

#### Scenario: An unreachable balance endpoint degrades to the backstop

- **WHEN** the balance endpoint cannot be reached
- **THEN** the fetch proceeds uncapped and reports a warning, and a
  payment-required response mid-run is handled by the partial-fetch salvage
