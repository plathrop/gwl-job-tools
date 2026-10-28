# Proposal

## Why

The paid feed's compensation data is unreliable on a non-trivial slice of
records, and scoring takes it at face value. On 2026-10-28's 100-credit run,
five leads carried `min_annual_salary_usd` values that are not real USD
salaries — 187.2M–208M (Advantech PM), 218.4M–301.6M (Velociti QC lead),
176.8M–185.1M (Mayfair property accountant), 1.56M–2.28M (WW Talents SWE),
and 2.08M–2.808M from a source string of `"$40,000 - $54,000 a week"`. The
compensation dimension interpolates the configured floor→ceiling and
saturates at 100 above the ceiling, so every one of those scores 100 with
full weight and lifts the composite. Junk postings rank into the review
queue ahead of real roles — the exact failure the review queue exists to
prevent, and it undermines the one thing the operator trusts: the order of
the list.

Why now: the paid feed made this a daily occurrence rather than a curiosity,
and the triage session on that same run (97 of 100 marked `ignore`) is the
cost. The fix is small, local to scoring, and repairable retroactively via
`rescore` — but only if it lands before more leads accumulate bad scores.

## What Changes

- **Implausible compensation becomes unknown compensation.** When a
  posting's compensation exceeds a plausibility cap, the compensation
  dimension drops out of the composite and the remaining weights
  renormalize — the same treatment an absent compensation range already
  gets. The lead is still ingested and still scored; it is never rejected.
- **The composite's breakdown says why**: the human-readable breakdown
  distinguishes "compensation unknown" from "compensation implausible
  (208,000,000 exceeds the 600,000 cap)".
- **The cap is config-derived and tunable**: a new
  `compensation_plausibility_multiplier` config key (default `2.0`) sets the
  cap as a multiple of the configured `compensation_ceiling`. With the
  current config that is a 600,000 USD/year cap. An invalid value fails at
  config load, not at scoring time.
- **Inverted ranges are normalized**: a range whose `min` exceeds its `max`
  is swapped before either the floor gate or the plausibility check
  compares it, so a data error cannot silently defeat both.
- **Retroactive repair**: `rescore` re-runs scoring with the current config,
  so existing leads with implausible compensation are re-scored correctly
  without touching the event log.
- **Spec**: a new `scoring` capability, covering only this behavior (the
  scoring dimensions themselves predate OpenSpec adoption and are not
  re-specified here).

## Capabilities

### New Capabilities

- `scoring`: how a posting's compensation range is judged plausible enough
  to score, and what the composite does when it is not.

### Modified Capabilities

None. `discovery` (fetch → extract → gate → score → persist) is unchanged:
this rule lives inside scoring, and `rescore` already re-runs scoring by
contract.

## Impact

- **Code**: `config` (`compensation_plausibility_multiplier` + validation),
  `domain/scoring` (`compensation_score`, breakdown text),
  `domain/gates` (shares the range normalization), `domain/events`
  (`CompRange` normalization helper), README config table.
- **No breaking changes**: absent config key = 2.0; event schema untouched;
  no gate semantics change (a lead is never rejected for implausible comp).
- **Behavior change with a visible consequence**: leads whose only defect
  was an inflated range will drop in rank on `rescore`, and `list` output
  will reorder accordingly. That is the intended effect.
- **Dependencies**: none new.
- **Observability** (decision 0011): the always-visible signal is the
  composite breakdown, which names the offending value and the active cap in
  ordinary `list`/`show` output with no configuration; `rescore` also reports
  how many scores changed. Additionally one `warn!` fires per refused range
  with identifying fields (company, title, min, max, cap, multiplier) — rare
  enough today (5 per 100 leads) to be signal. Honest limit, verified in
  code: with telemetry off (the default) the file sink's floor is `error`, so
  that warn is visible only at a raised log level; the underlying filter
  policy is filed separately as GWLJ-5ca8pg rather than smuggled into a
  scoring change.
- **Related but out of scope**: the same untrustworthy upstream field feeds
  the `min_salary_usd` server-side filter (GWLJ-ieqxvw), and a future
  improvement can annualize from the source-published
  `salary_currency`/`salary_period`/`min_salary`/`max_salary` fields where
  present. Both are recorded on the pebble, not attempted here.
- **Pebble**: GWLJ-f5pdka.
