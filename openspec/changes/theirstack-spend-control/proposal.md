# Proposal

## Why

The 2026-10-07 live incident (GWLJ-w9xhcg) exposed two spend-control gaps
beyond the salvage fix:

1. **No budget awareness.** The adapter pages until the account is empty —
   it burned 1,500 credits in one run — and with a balance below one page's
   credit requirement it is bricked on page 0 (every request 402s). The
   credit-balance endpoint (`GET /v0/billing/credit-balance`, free) exists
   precisely for this and was deferred in `theirstack-paid-feed`.
2. **No way to narrow the query.** The default query pushes only the
   blacklist and recency window server-side, so a paid run buys the entire
   TheirStack index for the window (~1,800 jobs over 14 days), the vast
   majority of which are guaranteed client-gate rejections — a
   ₱18k/month tech-support agent is a credit spent to be rejected by the
   $225k compensation floor. TheirStack is shaped for targeted search (30+
   documented filters — titles, technologies, seniority, country, salary,
   workplace type, description patterns), and its own docs recommend
   building the query in the app UI and copying the cURL. The tool needs a
   way to spend credits only on plausible jobs — and, since it will be
   open-source, that way must not hard-code one person's job search.

## What Changes

- **Configurable server-side query filters**: a generic
  `[sources.theirstack.query]` TOML table, merged into the search request
  body. Any documented TheirStack filter parameter may be set (arrays,
  strings, booleans, numbers); keys the adapter itself owns (pagination,
  recency, watermark, blacklist, strict-mode gates) are rejected at config
  load with a clear error. `${VAR}` interpolation applies to the table like
  everywhere else.
- **Budget-aware pagination**: before each page the adapter checks the
  credit balance and caps the page `limit` to what the balance can pay for,
  so a small balance is a small fetch instead of a bricked source. When the
  balance runs out mid-run, the fetch stops as a partial fetch (design
  decision 8 of `theirstack-paid-feed`): records already paid for are kept
  and the shortfall is reported.
- **Per-run spend bound**: an optional `[sources.theirstack]
  max_credits_per_run` caps what a single run will spend regardless of
  balance (default: no cap — the balance is the only bound).
- **Spend visibility**: the run summary reports credits spent per paid
  source (records returned = credits, per TheirStack's pricing); the
  source-fetch span records that spend, and per-page balance checks log at
  `debug!` (balance remaining, cap applied, running spend).
- **Spec**: ADDED requirements for query-shaping config, budget-capped
  paid fetches, and spend reporting in the `discovery` capability.

**Supersedes** (narrowly) the `theirstack-paid-feed` decision that "skills
are never a query filter": that rule was about *defaults* — the adapter must
not silently narrow the feed, because skills are a score dimension, not a
gate. An explicit operator-supplied `query` table is an opt-in spend
decision, not a silent filter: the default query is unchanged, and what a
filtered feed returns still flows through every client-side gate and the
score.

## Capabilities

### New Capabilities

None.

### Modified Capabilities

- `discovery`: a paid source's query is operator-shapeable via config; paid
  fetches are bounded by the credit balance (and optionally a per-run
  budget); the summary reports per-source credit spend.

## Impact

- **Code**: `config` (`[sources.theirstack]` gains `query` + `max_credits_per_run`),
  `discovery/theirstack` (query merge + budget loop), `commands/discover`
  (summary field), README.
- **Dependencies**: none new (reqwest/serde/serde_json/regex already in use).
- **No breaking changes**: config additions are opt-in (absent = current
  behavior); the event schema is untouched.
- **Observability** (decision 0011): the source-fetch span records
  `credits_spent`; per-page balance checks log at `debug!` (balance
  remaining, cap, running spend); a budget stop logs at `error!` with the
  reason via the partial-fetch mechanism; the summary carries
  `credits_spent_by_source` so a `--json` consumer can track spend per run.
