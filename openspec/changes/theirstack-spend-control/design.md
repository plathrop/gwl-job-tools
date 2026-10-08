# Design

## Context

`theirstack-paid-feed` (still unarchived; amended by GWLJ-w9xhcg) wired the
TheirStack Jobs API behind `[sources.theirstack]` with salvage-on-402 and
failure-reason surfacing. Two spend-control gaps remain, both measured by
the 2026-10-07 incident:

- The adapter has no budget awareness: it pages until the account empties
  (1,500 credits in one run), and a balance below one page's requirement
  (500) bricks the source — every page-0 request 402s.
- The query is untargeted by design: only the blacklist and recency window
  are pushed server-side, so the run buys the whole index window (~1,800
  jobs / 14 days), most of which the client gates then reject. The
  credit-balance endpoint exists and was explicitly deferred.

TheirStack's search endpoint documents 30+ filter parameters (titles,
technologies, seniority, country, salary, workplace type, description
patterns, …), and the vendor's own recommended workflow is "build the query
in the app UI, copy the cURL." The tool will be open-sourced; the filtering
surface must serve searches this repo cannot predict.

## Goals / Non-Goals

**Goals:**

- An operator can shape the paid query via config — any documented filter,
  not a hand-picked subset.
- A paid fetch never requests more than the balance can pay for; a small
  balance is a small fetch, not a bricked source.
- An optional per-run spend bound.
- Spend is visible: per-source credits in the summary, budget/balance facts
  in the logs.

**Non-Goals (deferred):**

- Typed first-class config for individual filters (`job_titles = [...]`,
  `technologies = [...]`) — the generic table subsumes them; promote a
  filter to typed config only when a user asks twice (the rule of three).
- Surfacing `earliest_expiration` (expiring credits) in the summary.
- Cursor-based pagination (`cursor` parameter) — page-based works.
- Per-source budget telemetry beyond the summary counts.

## Decisions

### 1. Query shaping is a generic pass-through table with reserved keys

`[sources.theirstack.query]` is a TOML table merged into the search request
body:

```toml
[sources.theirstack.query]
job_seniority_or = ["senior", "staff"] # example — any documented param
job_technology_slug_or = ["kubernetes"]
job_country_code_or = ["US", "CA"]
```

- **Merge order**: adapter-generated parameters first, then the table's —
  with the reserved-key rule below making true precedence a non-event.
- **Reserved keys, unconditionally rejected** (config load fails, no
  credits spent): `limit`, `page`, `cursor`, `posted_at_max_age_days`,
  `discovered_at_gte`, `company_name_not`, `company_domain_not` — these are
  owned by the pagination loop (page and cursor alike), the recency config,
  the watermark cursor, and the blacklist pre-filter. Conflicting with them
  would silently break the credit-budgeting machinery this change exists to
  protect.
- **Conflict-rejected** (only when `strict_filtering = true`):
  `workplace_types_or`, `min_salary_usd` — these are the strict-mode gates.
  With `strict_filtering = false` they are free for the operator, which is
  how you express e.g. `workplace_types_or = ["remote", "hybrid"]` (a shape
  strict mode cannot express).
- **Validation location**: adapter construction (`Theirstack::new`), so
  `build_sources` fails the run before any network I/O.
- **Interpolation**: config-wide `${VAR}` interpolation already runs over
  the whole TOML tree, so query values reference env vars like any other
  string.

*Why generic*: a typed subset (titles + technologies, say) bakes in 2026's
guess about what matters, ages badly against a vendor API that adds filters
monthly, and cannot serve an open-source user whose search is nothing like
ours (the Philippine tech-support market is a *feature* for someone).
TheirStack's own UI-to-cURL workflow means the table's contents are
copy-paste from the vendor's docs. *Alternative*: hand-picked typed fields —
rejected for exactly the aging/subset reason; the query table is the
typed-field superset.

**Supersedes, narrowly**: `theirstack-paid-feed`'s "skills are never a query
filter." That rule was about defaults — the *adapter* must not silently
narrow the feed, because skills are a score dimension, not a gate. An
operator-supplied table is an explicit spend decision: the default query is
unchanged, and everything a narrowed feed returns still flows through every
client-side gate and the scoring dimensions.

### 2. Budget-aware pagination: check the balance, cap the page

Live-verified demand model (GWLJ-w9xhcg evidence): a page request requires
`min(limit, remaining matches)` credits and 402s otherwise. Capping each
page's `limit` to the remaining balance therefore guarantees the request
succeeds — a small balance becomes a small fetch.

The loop, per page:

1. `GET /v0/billing/credit-balance` (free, Bearer auth). Remaining =
   `api_credits - used_api_credits`. A zero balance stops the fetch as a
   partial fetch (records already paid for are kept; reason:
   "credit balance exhausted before page N"). A failed balance check does
   **not** block the fetch: log `warn!`, proceed uncapped — the payment-
   required response and the GWLJ-w9xhcg salvage are the backstop, and
   discovery should not fail because a billing endpoint blinked.
2. `page_limit = min(PAGE_SIZE, remaining_balance)`.
3. If `max_credits_per_run` is set: `budget_left = budget - spent_this_run`;
   zero stops the fetch as a partial fetch (reason: "per-run credit budget
   reached"); otherwise `page_limit = min(page_limit, budget_left)`.
4. Request the page with `limit = page_limit`; `spent += records returned`.

**Termination** changes with the capped page size: a page ends the loop when
it returns fewer records than *its own capped limit* (not `PAGE_SIZE`), so a
capped-but-exact final page (count == page_limit) continues to the next
iteration, where the balance/budget check stops it cleanly with a reason.

*Alternative*: parse the 402's "Required: N credits" and retry with that N —
rejected: it spends the response budget discovering what the free balance
endpoint already says, and races against concurrent spenders of the same
account.

### 3. Spend is reported, not just bounded

`SourceBatch` gains `credits_spent: u64` (records a paid source returned this
fetch; `0` for free sources — one credit per record is TheirStack's pricing,
so the count *is* the spend). The driver folds it into
`BatchSummary.credits_spent_by_source` (per-source, BTreeMap) — printed on
the human path, serialized under `--json`. The source-fetch span records it,
along with the adapter's per-page balance checks (`debug!` with
balance/cap/spent) and the stop reasons (which flow through the existing
`error!` partial-fetch path).

The `discovery` event payload is NOT extended this time: the run summary and
the event already record outcome counts and failure reasons; per-source
spend is derivable (records fetched per source) and the summary is the
operator-facing surface.

### 4. Config surface

```toml
[sources.theirstack]
max_credits_per_run = 750 # optional; absent = balance is the only bound

[sources.theirstack.query] # optional; absent = default query
job_seniority_or = ["senior", "staff"]
```

`max_credits_per_run: Option<u64>` and
`query: Option<BTreeMap<String, toml::Value>>` on `SourceConfig` (serde
default = absent; Remotive ignores both). Unknown top-level keys remain
rejected by `deny_unknown_fields`; the query table's keys are
source-API parameters, validated only against the reserved list above —
typos surface as the vendor API's own errors (which the reason-surfacing
path now reports).

## Risks / Trade-offs

- **Vendor-shape coupling**: a wrong key in the table surfaces as a
  vendor 4xx on a real (paid-triggering) request. Mitigation: the 400-style
  failures on page 0 are whole-source `Err` with the vendor's message in
  the reason — one bad run, no silent data loss.
- **Balance-check latency**: one extra GET per page (free, ~300ms politeness
  delay each). Paid rate limit is 4 req/sec; the doubled request count stays
  within it. Acceptable for a job-search cadence, not a hot loop.
- **Stale balance**: a concurrent consumer of the same account can drain
  credits between the check and the page request → a 402 mid-run, handled
  by the salvage path. Defense in depth, not a new failure mode.
- **Budget undercount**: credits are counted as records returned; a vendor
  pricing change (per-request fees) would undercount. TheirStack's model
  is per-record today; the field is named `credits_spent_by_source` to make
  the assumption visible.

## Migration Plan

Additive and opt-in: no `query` table and no `max_credits_per_run` means
exactly the pre-change behavior except the balance cap (which only ever
*shrinks* a request that would otherwise 402). No event-schema change.
Rollback = revert.

## Observability

Per decision 0011 and the change's PR gate: the source-fetch span records
`credits_spent`; the adapter logs each balance check at `debug!`
(balance_remaining, cap) and budget/balance stops at `error!` through the
partial-fetch reason path; the run summary carries
`credits_spent_by_source` (human-printed and `--json` serialized). A
`--dry-run` of a paid source answers "what would this spend" — capped by
the same balance/budget logic as a real run.
