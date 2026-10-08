# Tasks

## 1. Offset-based continuation

- [x] 1.1 Replace the `page` request parameter with `offset` (records already fetched) in `fetch_jobs`; the zero-balance arbitration ask continues at the fetched offset. Verify: transport tests assert `"offset"` in request bodies and no `"page"` key; the paginates test asserts offset 0 then 500.
- [x] 1.2 Budget-check `debug!` event carries the offset alongside page_limit/balance/spent. Verify: unit test not required (log field), covered by review.

## 2. Faithful model + prefix property

- [x] 2.1 `ModelApi` derives the serving window from the request (`offset` + `limit`), enforces the demand contract from the window start (`min(limit, matches − offset)` credits, else 402 charging nothing), and records a violation for any `page`-carrying request. Verify: a transport-level unit test drives the model with an explicit multi-page varying-cap scenario and asserts the contiguous-prefix outcome.
- [x] 2.2 The budget-loop proptest gains the prefix property: the fetched records' result-set indices are exactly `[0, spent)` — no duplicates, no skips. Verify: the property fails against the pre-fix loop (verified manually) and passes after; proptest cases green.
- [x] 2.3 Deterministic tests: the 758/758 boundary case (balance == matches > page size) via the model — complete fetch, exact contiguous window, no failure; the 199-balance case's arbitration ask asserts `offset` and the 402 short-fall path; the zero-balance-before-anything case unchanged.

## 3. Finishing

- [x] 3.1 Run `cargo fmt`, `dprint fmt`, `cargo clippy --all-targets -- -D warnings`, `cargo test`, and `openspec validate theirstack-offset-pagination --strict`. Verify: all exit 0.
