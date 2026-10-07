# Tasks

## 1. Snapshot-capable discovery seam

- [x] 1.1 Change `DiscoverySource::postings()` to `fetch() -> Result<SourceBatch>` (add `SourceBatch { outcomes, failed }`, remove `Posting` and its hints struct). Verify: crate compiles; `cargo check` clean.
- [x] 1.2 Move per-URL re-extraction into the Remotive adapter: it now calls `ingest_url` per posting and counts per-posting failures in `SourceBatch.failed`. Verify: existing Remotive parse tests still pass; a new unit test asserts a dead posting URL is counted in `failed`, not returned as `Err`.
- [x] 1.3 Update the `discover` driver to consume `SourceBatch` (flatten `outcomes`, sum `failed`) instead of the old `fetch_sources` + `fetch_postings` split. Verify: `discover` command unit tests pass; `fetch_postings` is removed or repurposed.
- [x] 1.4 Update discovery tests for the new seam and prove Remotive parity (ingest → gate → score → dedupe unchanged). Verify: `cargo test` green with the refactor; within-batch dedup test still passes.

## 2. TheirStack adapter

- [x] 2.1 Add `api_key`, `posted_at_max_age_days` (default 30), and `strict_filtering` (default false) to `SourceConfig`, and add config-wide `${VAR}` environment interpolation to the config loader (a single pass over the parsed TOML before deserialization; a missing var resolves to empty). Remotive ignores these fields; unknown keys are rejected. Verify: config tests cover each field, the interpolation, and the missing-var → empty behavior.
- [x] 2.2 Implement the TheirStack adapter: `POST /v1/jobs/search` (Bearer `THEIRSTACK_API_KEY`, serial pagination, `limit` 500) returning `SourceBatch`. Verify: a unit test over a saved API fixture asserts parsed outcomes (url, description, salary, remote, company, technology_slugs).
- [x] 2.3 Map each record to an `IngestOutcome` (`adapter`/`source` = `theirstack`, `url` = `final_url` canonicalized, `raw_text` = `description`, `extracted` = structured fields); a missing `THEIRSTACK_API_KEY` returns `Err` so the source is reported failed. Verify: a unit test asserts the missing-key path returns `Err`, and a fixture maps to the expected `IngestOutcome`.
- [x] 2.4 Shape the query: blacklist → `company_name_not`/`company_domain_not` and recency → `posted_at_max_age_days` always; behind `strict_filtering`, also push `remote_only` → `workplace_types_or: ["remote"]` and `compensation_floor` → `min_salary_usd`; never push skills. Verify: unit tests assert the default query carries blacklist + recency only, and the strict query adds the remote/comp filters.
- [x] 2.5 Trust-but-verify: run `extract_fields` over `description` and emit a per-mismatch span carrying the structured vs extracted values (`comp_structured`/`comp_extracted`, `remote_structured`/`remote_extracted`), keyed by url + source (the lead id is not minted until ingest); the comp comparison ignores `raw`. Verify: unit tests assert a salary/remote mismatch emits the span fields, and that raw-formatting differences alone do not fire a mismatch; proptest property tests pin the comp-comparison semantics.
- [x] 2.6 Register `theirstack` in the source registry (`make_source`) so `build_sources` can construct it. Verify: a unit test asserts `build_sources` with `[sources.theirstack]` yields the adapter, and an unknown name still errors.
- [x] 2.7 Emit source null-rate telemetry: the source-fetch span records unknown-workplace, unknown-salary, and strict-would-drop counts; the run summary records those plus client-gate rejection counts (aggregate and per-source — gating runs after the fetch span closes). Verify: a unit test asserts the summary/span fields are populated from a fixture feed.
- [x] 2.8 Add `charges_per_record()` to `DiscoverySource` (default `false`; `true` for TheirStack) and gate a paid `--dry-run`: prompt `[y/N]` on stderr before fetching, `--yes` to skip, refuse in non-TTY/`--json` (unless `--yes`). Verify: a unit test asserts the prompt/refuse/`--yes` paths.

## 3. Credit budgeting: watermark + credit exhaustion

- [x] 3.1 Add a per-source `discovered_at` watermark to the `discovery` run-event payload (additive, optional field). Verify: the existing discovery-event test still passes; a new test round-trips the watermark through append → replay.
- [x] 3.2 Store the run's max `discovered_at` per source on the run event, and derive the prior watermark from the last run event for that source; pass it as `discovered_at_gte` on the next TheirStack query. Verify: an integration test runs twice and asserts the second query carries the first run's watermark.
- [x] 3.3 Surface `metadata.truncated_results` in the `BatchSummary` (new field, default 0) and print it. Verify: a unit test maps a truncated response into the summary; the summary serializes the count.
- [x] 3.4 Confirm `truncated_results` semantics against a live near-exhausted response before relying on them (a compatible-schema product reads the field the opposite way). Tracked as GWLJ-2yflze.
      Settled 2026-10-07 with live near-exhaustion data: exhaustion does NOT surface as `metadata.truncated_results` (every page reported 0); the API demands credits ≥ `min(limit, remaining matches)` per request and 402s otherwise, with required/available counts in the error body. Follow-ups: tasks 6.1–6.4.

## 6. Partial-fetch salvage + failure-reason surfacing (GWLJ-w9xhcg, live-incident follow-up)

- [x] 6.1 Expose non-2xx response bodies through the client: add a POST method that returns `(status, body)` without bailing, sharing the existing politeness/retry loop, so adapters can mine error payloads (TheirStack's 402 credit message). Verify: client unit test asserts a 402 body is returned, not discarded; the retry loop still honors 429/503.
- [x] 6.2 Salvage in `fetch_jobs`: on a page error after records have been fetched, return the fetched pages with the failure reason (best-effort: parse `Required: N API credits` from the 402 body and add N to `truncated_results`); a failure before any record is fetched stays `Err`. Verify: a scripted two-page transport test (page 0 ok, page 1 402 with the live E-007 body) yields the page-0 records, `truncated_results` = N, and the partial reason; a page-0-only 402 yields `Err`.
- [x] 6.3 Carry `partial_error` on `SourceBatch`; the discover driver ingests a partial batch's outcomes, records its watermark, lists the source in `failed_sources`, and passes the reason through. Verify: unit tests over the driver flatten loop (partial batch → outcomes ingested + source listed + reason carried; `Err` batch → source listed with reason).
- [x] 6.4 Add `failed_source_reasons` (source → reason) to `BatchSummary` and the `discovery` run-event payload (additive, optional), print reasons in the human summary, and log whole-source failures at `error!` instead of `warn!`. Verify: summary serialization test; run-event round-trip test asserts the reasons map; the failed-source log line emits at error level with the source and reason.

## 4. Decision telemetry (separate small PR, outside increments A/B/C)

- [x] 4.1 Instrument the review path for correlation: the ingest span carries `remote_known`/`comp_known`, and the mark/review spans carry the mark plus the lead's ingest-time status, so Honeycomb can answer "does the operator pursue unknown-status leads?". Verify: a test asserts the mark span carries the fields, and `cargo clippy --all-targets -- -D warnings` stays clean.

## 5. Finishing

- [x] 5.1 Run `cargo fmt`, `dprint fmt`, `cargo clippy --all-targets -- -D warnings`, and `cargo test`; confirm all pass. Verify: all three commands exit 0.
- [x] 5.2 Run `openspec validate --strict` and fix any failures. Verify: `openspec validate` reports the change valid.
