# Design

## Context

`compensation_score()` in `src/domain/scoring.rs` takes the top of the quoted
range (`comp.max.or(comp.min)` — the same value the compensation-floor gate in
`src/domain/gates.rs` compares), interpolates it linearly across
`compensation_floor`→`compensation_ceiling`, and clamps the result to
`[0, 100]`. Above the ceiling every value scores 100, so the dimension cannot
distinguish a 350k package from a 208M data error — and the paid feed
produces the latter regularly (measured: 5 of 100 records on 2026-10-28).

Three facts constrain the fix:

1. `ExtractedFields` is stored verbatim in the event log. Scoring is derived
   state, re-derivable by `rescore`; the ingest payload is not.
2. Gate philosophy (module docs, `gates.rs`) is that gates reject only on
   high-confidence negatives, because a false rejection is an invisible loss.
3. The tool is intended to be usable by people whose compensation scale is
   not the maintainer's, so a hard-coded dollar cap is not shippable.

## Decisions

### 1. The rule lives in scoring — not the adapter, not a gate

`compensation_score()` returns `None` for an implausible range, which is
exactly the "unknown compensation" path already implemented: the dimension is
omitted and the remaining weights renormalize.

Rejected alternatives:

- **Null the value in the adapter.** Would launder what the source said out of
  the only durable record we have, and `rescore` could never repair leads
  already ingested.
- **Reject in the compensation-floor gate.** A $600k+ package is not an
  invalid posting; the permissive-gate philosophy forbids converting "we
  don't trust this number" into a rejection.
- **Clamp to the ceiling instead of dropping.** Clamping preserves the exact
  100 that causes the inflation, just with more arithmetic. "Unknown" is the
  honest encoding of "this number is not usable".

### 2. Cap = `compensation_ceiling × compensation_plausibility_multiplier`

New config key, default `2.0` (a 600,000 USD/year cap on the current config).
Deriving from the operator's own ceiling rather than a global constant is what
makes this shareable: the ceiling is already the operator's statement of what
"top of the market" means to them.

Validation at config load (`Config::validate`, alongside the existing
floor-below-ceiling check):

- non-finite (`NaN`, `inf`) → error naming the key;
- `< 1.0` → error naming the key. At `1.0` the rule only fires above the
  ceiling; below that it would silently drop every at-or-above-ceiling range,
  which is a footgun disguised as configuration.

When `compensation_ceiling` is unset, `compensation_score` already returns
`None`, so the cap is never computed.

### 3. Compare `max.or(min)`, matching the gate

Using a different representative value in scoring than in the gate would let
the two disagree about the same posting. One comparison value, documented once.

### 4. Inverted ranges normalize in `CompRange`, not in each consumer

Add `impl CompRange { fn normalized(self) -> CompRange }` in
`src/domain/events.rs` (where the type lives), swapping bounds when
`min > max`. Both `gates::evaluate` and `scoring::score` call it.

A single-component fix was rejected: normalizing only in scoring would leave
the gate comparing an inverted range, so a record could be *rejected* by the
floor gate while scoring treats it as plausible — two components disagreeing
about the same lead, with the rejection invisible.

### 5. The breakdown carries the reason, so the drop plumbing gains a reason

`composite_and_breakdown` currently takes `dropped: Vec<&'static str>` and
renders fixed notes ("compensation: unknown, weight renormalized"). An
implausible drop needs its numbers:

```
39 = 0.34·level(50) + 0.283·skills(10) + 0.377·remote(50) [compensation: implausible (208000000 exceeds cap 600000), weight renormalized]
```

Implementation: replace the string list with a small enum (e.g.
`Dropped { name, reason }` where `reason` is `Unknown | Implausible { value,
cap } | NoResume`) and render from it. This is internal to `scoring`, no
schema or API change, and it keeps the existing strings for the existing
cases byte-identical where tests already assert them.

### 6. Observability: one `warn!` per refused range, no new counters

```
warn!(company, title, min, max, cap, multiplier, "compensation implausible; dimension dropped")
```

Expected frequency is low (5/100 leads on the worst day measured, none on
most), which makes `warn` signal rather than the wallpaper an `info` would
become. Deliberately **not** added:

- a summary/discovery-event counter — plumbing a score outcome back into the
  discovery payload for a rare event;
- promoting the existing `theirstack_mismatch` comp span (currently `debug!`)
  — that signal answers a different question (structured field vs text-quoted
  comp) and would fire on ordinary disagreements; recorded as a follow-up on
  GWLJ-f5pdka rather than bundled here.

## Risks and trade-offs

- **A genuine high-paying role loses its compensation dimension.** At the
  default cap that means total comp above 600k. Accepted by the maintainer
  ("a $600k total comp is enough that I don't care if we drop something
  higher"), and tunable for anyone who disagrees. The lead still appears and
  still ranks on level/skills/remote; it simply loses a saturated 100.
- **Existing scores do not change until `rescore` runs.** This is correct
  (scores are recorded history), and the change's README note must say so, or
  the operator will think the fix did nothing.
- **The upstream data problem is not fixed.** `min_salary_usd` in the paid
  query carries the same untrustworthy field, so the server-side filter
  narrows less than it appears to (GWLJ-ieqxvw). Out of scope here.

## Non-goals

- Re-specifying the existing scoring dimensions, weights, or saturation
  behavior. The repository predates OpenSpec adoption; the new `scoring`
  capability covers only the behavior this change introduces.
- Annualizing from TheirStack's source-published `salary_currency` /
  `salary_period` / `min_salary` / `max_salary` fields. Correct in principle,
  but ~2.5% fill and only for jobs discovered since 2026-10 (docs checked
  2026-10-28). Recorded on the pebble.
- Anything about gates, ranking weights, or the review UX.

## Testing

- Unit tests pinned to the five real records from 2026-10-28 (208M, 301.6M,
  185.1M, 2.808M, 2.28M) asserting each drops the dimension, plus a
  350k record asserting it still scores 100.
- Unit tests for the inverted-range normalization, asserting the gate and
  scoring agree on the same normalized value.
- Config validation tests: `NaN`, `inf`, `0.5`, and an accepted `3.0`.
- Property tests: (a) any value above the cap produces the same result as
  `comp: None`; (b) within `[floor, cap]` the score is monotone
  non-decreasing in value; (c) `normalized(min>max)` equals
  `normalized(max>min)`.
- An integration test that ingests a fixture with an implausible range,
  rescores it, and asserts the recorded score loses the dimension.
