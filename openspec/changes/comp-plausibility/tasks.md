# Tasks

## 1. Config: plausibility multiplier

- [ ] 1.1 Add `compensation_plausibility_multiplier` to `Config` accepting an integer or float literal (absent = 2.0, resolved through a getter so callers never handle the default). Verify: config tests cover absent, `2` (integer literal), `2.0` (float literal), and `${VAR}`-free parsing; an unknown sibling key is still rejected.
- [ ] 1.2 Validate it in `Config::validate`: non-finite or `< 1.0` fails with an error naming the key, alongside the existing floor-below-ceiling check. Verify: unit tests for `NaN`, `inf`, `0.5` (each an error naming the key) and `3.0` (accepted).

## 2. Compensation range normalization

- [ ] 2.1 Add `CompRange::normalized(self)` in `src/domain/events.rs`, swapping bounds when `min > max` and leaving everything else untouched. Verify: unit tests for the swapped, already-ordered, and single-bound (only `min`, only `max`) cases.
- [ ] 2.2 Call it from both consumers: the compensation-floor branch of `gates::evaluate` and `compensation_score`. Verify: a test asserting an inverted range yields the same gate outcome and the same score as the correctly-ordered equivalent.

## 3. Scoring: implausible compensation drops the dimension

- [ ] 3.0 Change `compensation_score` to return a verdict (`Scored(u64) | Unknown | Implausible { value, cap }`) instead of `Option<u64>`, and update the existing `Option`-asserting tests to the new shape without changing their expectations. Verify: `cargo test domain::scoring` passes with only the assertion shape changed (the pre-existing expected scores are untouched).
- [ ] 3.1 Compute the cap in the scoring path (`ceiling × multiplier`) and return `Implausible`, so the dimension is dropped and the reason carried, when the compared value (`max.or(min)`) exceeds it. Verify: unit tests pinned to the five real 2026-10-28 records (208,000,000 / 301,600,000 / 185,120,000 / 2,808,000 / 2,280,000) each drop the dimension; a 350,000 record still scores 100.
- [ ] 3.2 Replace `dropped: &[&'static str]` with a reason-carrying form (`Unknown | Implausible { value, cap } | NoResume`) and render it, keeping the existing unknown/no-resume strings byte-identical. Verify: `composite_and_breakdown` unit tests — existing assertions unchanged, plus one asserting the implausible string carries both the value and the cap.
- [ ] 3.3 Property tests: (a) any compared value above the cap scores identically to `comp: None`; (b) within `[floor, cap]` the dimension is monotone non-decreasing; (c) `normalized(min>max) == normalized(max>min)`. Verify: `cargo test` runs them, and each property fails if the guard is removed.
- [ ] 3.4 Gate neutrality: assert `gates::evaluate` produces no failure for an above-cap compensation range (the spec's "never rejected, down-gated, or excluded" clause). Verify: the test fails if a gate ever starts rejecting implausible comp.

## 4. Observability

- [ ] 4.1 Emit one `warn!` when a range is refused, with identifying fields (`company`, `title`, `min`, `max`, `cap`, `multiplier`). Verify: a test captures the fields for a refused record, and a plausible record emits nothing.
- [ ] 4.2 Document the honest limit where it matters — in the code comment beside the `warn!` and in the README config note — that with telemetry off the sink's floor is `error`, so the warn is visible only at a raised `--log-level`; the always-visible signal is the breakdown string (GWLJ-5ca8pg tracks the filter policy). Verify: the README note exists and the test for task 4.1 uses an explicit level rather than assuming the default admits warns.

## 5. Retroactive repair path

- [ ] 5.1 Integration test: ingest a fixture with an implausible range, run `rescore`, and assert the new `scored` event omits the compensation dimension while the original `scored` event stays in the log. Verify: the test asserts two `scored` events with different revisions (or revision bump) and different breakdown strings.
- [ ] 5.2 Confirm `rescore`'s human output reports how many scores changed, so the operator can see the effect of this change on existing leads. Verify: a CLI test asserting the reported count for a two-lead fixture where one changes.

## 6. Docs and finishing

- [ ] 6.1 README: document `compensation_plausibility_multiplier` in the config table, and add a short note that existing leads need `rescore` to pick up the new rule. Verify: the README's config table lists every key `Config` parses (spot-check against `config.rs`).
- [ ] 6.2 Run `cargo fmt`, `dprint fmt`, `cargo clippy --all-targets -- -D warnings`, `cargo test`, and `openspec validate comp-plausibility --strict`. Verify: all exit 0.
- [ ] 6.3 Record the outcome on GWLJ-f5pdka: what shipped, the measured before/after on the 2026-10-28 batch, and the deferred follow-ups (`salary_period`-based annualization, mismatch-signal promotion). Verify: the comment exists on the pebble and the pebble closes with the PR.
