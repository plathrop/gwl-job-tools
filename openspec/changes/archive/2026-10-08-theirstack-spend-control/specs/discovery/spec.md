# Spec Delta

## ADDED Requirements

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
so a small balance yields a small fetch rather than a rejected one. When the
balance cannot pay for any further records, the fetch SHALL stop as a
partial fetch: records already paid for are ingested, and the reason is
carried per the source-failure-reason requirement. A balance-check failure
SHALL NOT block the fetch — the payment-required response and the
partial-fetch salvage are the backstop.

#### Scenario: A small balance yields a bounded fetch

- **WHEN** a paid source is fetched and the account's remaining credits are
  fewer than the source's page size
- **THEN** the page request's size is capped to the remaining credits, the
  records those remaining credits pay for are fetched and ingested, and the
  fetch reports that it stopped early because the balance ran out

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
