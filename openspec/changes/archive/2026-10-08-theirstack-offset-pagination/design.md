# Design

## Context

`theirstack-spend-control` capped each page's `limit` to what the balance
and budget could pay for — but kept the adapter in TheirStack's **page-based
pagination** (`page` + `limit`). PR #46's review (Copilot + deepseek-v4-pro)
found, and live probing confirmed (2026-10-08, GWLJ-nz0fs5), that the
server-side window for page-based requests is `page × limit`:

- `limit:1, page:1` → global index 1
- `limit:3, page:1` → global indices **[3,6)** — not [1,4)

So a varying `limit` duplicates and silently skips records — live on main
for any multi-page run where the cap varies (balance or budget between one
page and the result set; single-page and constant-`limit` runs, including
the original incident's 500/500/500 fetch, were unaffected). PR #46's
zero-balance "arbitration ask" inherited the same defect and could turn a
visible failure into silent missing records. The budget-loop `ModelApi`
concealed all of it by serving cumulatively off its own counter and
ignoring the request's `page` — the exact fixture-lies failure class the
model exists to prevent, now inside the model.

TheirStack's API documents a separate **offset-based mode**: `offset` =
"number of results to skip. Required for offset-based pagination", and the
vendor's own periodic-fetch examples use `offset` + `limit`. Live-verified:
`offset:1, limit:3` → indices [1,4).

## Goals / Non-Goals

**Goals:**

- Multi-page paid fetches are correct under a varying cap: no duplicated
  records, no skipped records.
- A completed fetch is never reported as failed (PR #46's original intent),
  and an incomplete fetch is never reported as complete.
- The `ModelApi` is faithful to the verified request semantics, so the
  proptest properties actually constrain the adapter's wire behavior.

**Non-Goals (deferred):**

- Cursor-based pagination (`cursor` param) — offset+limit suffices; the
  reserved-key list already blocks operator interference.
- Re-probing semantics for other endpoints — the probe evidence in this
  doc covers `/v1/jobs/search` only.

## Decisions

### 1. Continue by offset, not page

Each page request carries `offset` = records already fetched and `limit` =
the payable cap (`min(PAGE_SIZE, balance, budget-left)`). The window is
then ` [offset, offset + limit)` ∩ matches — correct by construction when
the cap varies, because the next request's offset advances by the records
actually returned, not by a page counter the server recomputes. The
adapter stops sending `page` entirely (the vendor's modes are distinct;
mixing them is undefined behavior we should not ship).

*Alternative*: hold `limit` constant and pad the final request — rejected:
a constant `limit` over the balance is exactly the unpayable request the
402 incident was about; the cap must vary, so the continuation must be
offset-based. *Alternative 2*: keep `page` and only ever request constant
`limit` pages, doing balance arithmetic in whole pages — rejected: it
over-pays when the balance cannot cover a full page (the bricked-source
bug `spend-control` fixed).

### 2. The zero-balance boundary is settled by the source's own response

Once records have been fetched and the balance reads zero, the adapter
issues one further request at `offset` = records fetched (capped only by
budget-left; the balance is known-empty). Under per-record pricing this
request is free either way:

- matches remain → the source rejects it with payment-required (E-007);
  the #44 machinery reports the shortfall (partial fetch, reason with the
  vendor's message, `truncated_results` absorbing the response's
  `Required: N` lower bound).
- nothing remains → the source returns an empty page; the fetch completes
  cleanly and no failure is reported.

This is PR #46's mechanism, made sound by offset semantics — and it is
strictly better than declaring exhaustion at the balance check, which is
the false-partial bug (a complete fetch reported as failed). A zero
balance *before any record is fetched* keeps its whole-source failure: the
spec's "no records, source failed, run continues" behavior, and no free
arbitration is owed to a fetch that never started.

### 3. The ModelApi derives its window from the request

The model now serves the window the request actually names:

- `offset` (default 0) + `limit` → serve `[offset, min(offset + limit,
  matches))` records, each carrying its result-set index;
- the demand contract is checked from the window start:
  `min(limit, matches − offset)` credits required, else a payment-required
  402 charging nothing;
- a request carrying `page` is a **violation** by construction — the model
  refuses to model page-based windows, so an adapter regression to `page`
  fails the property suite immediately.

The proptest gains the property this enables and this bug class needs:
**the fetched records are exactly the contiguous prefix `[0, spent)` of the
result set** — no duplicates, no skips, no gaps. The old model (cumulative
counter, `page` ignored) could not even express it.

### 4. Termination under offset semantics

A page ends the loop when it returns fewer records than its own capped
`limit` (unchanged). A page that exactly fills its cap continues; the next
iteration's balance/budget pass decides whether more is payable — and at a
zero balance, decision 2's arbitration settles it. The archived
`spend-control` design's termination model assumed cumulative
continuation; offset semantics are what make that model true.

## Risks / Trade-offs

- **Result-set churn between pages**: postings are inserted continuously;
  a re-issued offset window can shift by what churned in. Bounded and
  accepted — the same exposure page-based pagination has; the watermark
  cursor (not pagination) is the dedup backstop.
- **Vendor changes window semantics**: the model would faithfully catch a
  semantic change (the prefix property fails), which is the point — the
  probe evidence in this doc tells the next person what was verified and
  when.
- **`page` removal**: harmless for the vendor (distinct modes, offset
  explicitly documented); the model enforces the invariant in tests.

## Migration Plan

Request-shape change only (`page` → `offset`), invisible to operators and
config; no event-schema change. Single-page behavior identical. Rollback =
revert.

## Observability

The per-page budget-check `debug!` event gains the `offset` field alongside
`page_limit`/`balance`/`spent`. The boundary arbitration outcome needs no
new instrumentation: it flows through the #44 machinery — a 402 becomes
the error-level partial-fetch log with the vendor's message, the summary's
`truncated_results` and `failed_source_reasons`, and the `discovery` event
payload; a clean completion reports nothing (which is the desired signal:
no failure where there is none).
