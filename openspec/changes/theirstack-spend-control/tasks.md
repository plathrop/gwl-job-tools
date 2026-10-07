# Tasks

## 1. Config: query table + per-run budget

- [x] 1.1 Add `max_credits_per_run: Option<u64>` and `query: Option<BTreeMap<String, toml::Value>>` to `SourceConfig` (both optional; Remotive ignores them; unknown top-level keys still rejected). Verify: config tests cover absent (= defaults), present, and `${VAR}` interpolation inside query values.
- [x] 1.2 Validate the query table at adapter construction: unconditionally reject `limit`, `page`, `posted_at_max_age_days`, `discovered_at_gte`, `company_name_not`, `company_domain_not`; reject `workplace_types_or` and `min_salary_usd` only when `strict_filtering` is on. The error names the reserved key and the config path. Verify: unit tests for each reserved key (and their allowance when strict mode is off), and that `build_sources` fails before any fetch.

## 2. Adapter: budget-aware pagination + query merge

- [x] 2.1 Merge the validated `query` table into `build_query` output (generated params first, table after). Verify: a unit test asserts a declared filter rides along the generated ones and the default query is byte-identical when no table is declared.
- [x] 2.2 Balance check: `GET /v0/billing/credit-balance` (Bearer) via the politeness client; remaining = `api_credits - used_api_credits`. On failure: `warn!` and proceed uncapped. Verify: a scripted transport test parses the live balance shape; a transport test asserts the uncapped fallback.
- [x] 2.3 Budget loop: per page, cap `limit` to `min(PAGE_SIZE, remaining balance, budget left)`; stop as a partial fetch (records kept, reason carried) when the balance or the per-run budget is exhausted; termination is `count < that page's capped limit`. Verify: scripted tests — (a) balance 199 → page limit 199, fetch completes as partial with the balance-exhausted reason; (b) `max_credits_per_run` smaller than the matches → budget stop reason and no further page requests; (c) balance-check failure → uncapped page requests.
- [x] 2.4 Carry `credits_spent` (records returned) on `SourceBatch`; the driver folds it into `BatchSummary.credits_spent_by_source` and prints it on the human path; the source-fetch span records it. Verify: a driver unit test folds spend per source; summary serialization test.

## 3. Finishing

- [x] 3.1 README: document `[sources.theirstack.query]` (with a realistic example), `max_credits_per_run`, and the balance-capping behavior.
- [x] 3.2 Run `cargo fmt`, `dprint fmt`, `cargo clippy --all-targets -- -D warnings`, `cargo test`, and `openspec validate theirstack-spend-control --strict`. Verify: all exit 0.
