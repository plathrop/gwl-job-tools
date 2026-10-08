# Proposal

## Why

Review of PR #46 (Copilot + deepseek-v4-pro) surfaced a live-verified
correctness bug in the paid-fetch loop shipped by `theirstack-spend-control`:
the adapter paginates with TheirStack's **page-based** mode (`page` +
`limit`), whose server-side window is `page × limit` — coherent only while
`limit` stays constant. The spend-control caps (`min(PAGE_SIZE, balance,
budget-left)`) make `limit` **vary across pages**, so any multi-page fetch
with a varying cap **duplicates and silently skips records**. Verified
against the live API (2026-10-08): `limit:1, page:1` returns global index 1,
while `limit:3, page:1` returns indices **[3,6)** — not [1,4).

PR #46's false-partial fix (a zero-balance "arbitration ask") inherited the
same broken pagination and made the failure *invisible*: past the true end
of the result set it reads an empty page and reports a complete fetch over
missing records. And the budget-loop `ModelApi` concealed all of it by
serving records cumulatively off its own counter and ignoring the request's
`page` — the fixture-lies failure class, baked into the model itself.

## What Changes

- **Offset-based continuation**: the paid fetch loop sends `offset` =
  records already fetched (with `limit` = the payable cap) instead of
  `page` + `limit`. TheirStack documents `offset` ("number of results to
  skip. Required for offset-based pagination") and it was live-verified.
  A varying cap is now correct by construction: each window starts where
  the last ended.
- **Zero-balance boundary arbitration, settled by the source**: mid-run
  (records already fetched), a zero balance issues one further *offset*
  request — free under the pricing either way. A payment-required (402)
  response reports the shortfall (partial fetch, truncated count); an
  empty response completes the fetch cleanly. A zero balance before any
  record is fetched keeps the existing whole-source failure.
- **The model becomes faithful**: `ModelApi` derives its serving window
  from the request's `offset`/`limit` (not its own counter), enforces the
  demand contract from the window start, and records a violation if the
  adapter regresses to `page`-based requests.
- **A no-duplicates/no-skips property**: the budget-loop proptest asserts
  the fetched records are exactly the contiguous prefix of the result
  set — the property that would have caught this bug class.

**Spec**: MODIFIED `discovery` requirement *"Paid fetches are bounded by
the credit balance"* (offset continuation + the boundary arbitration
exception), because the shipped requirement's unconditional
"zero balance ⇒ partial fetch" is exactly the false-positive PR #46 set
out to fix — landing through a change, not code-only.

## Capabilities

### New Capabilities

None.

### Modified Capabilities

- `discovery`: paid multi-page fetches continue by offset; the zero-balance
  boundary is settled by the source's own response.

## Impact

- **Code**: `discovery/theirstack` (the fetch loop + its tests), no config
  or event-schema changes.
- **Dependencies**: none new.
- **No breaking changes**: the request shape changes (`page` → `offset`)
  but is invisible to operators; single-page behavior is identical.
- **Observability** (decision 0011): the per-page budget-check `debug!`
  event gains the `offset`; the boundary arbitration outcome flows through
  the existing failure-reason machinery (402 reason + truncated count, or
  clean completion). Live window semantics recorded in design.md so the
  next person doesn't have to re-probe.
