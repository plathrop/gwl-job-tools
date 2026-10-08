# Spec Delta

## MODIFIED Requirements

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
