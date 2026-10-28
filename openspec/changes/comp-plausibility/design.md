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

`compensation_score()` stops returning `Option<u64>` and returns a small
verdict instead — `Scored(u64) | Unknown | Implausible { value, cap }` — so
the caller can drop the dimension *and* say why (Decision 5). The drop itself
is the existing "unknown compensation" path: the dimension is omitted and the
remaining weights renormalize. The signature change is deliberate and ripples
into the existing `Option`-asserting unit tests in `scoring.rs`, which must be
updated as part of this change; a bare `None` cannot carry the value and cap
the breakdown needs.

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
- `< 1.0` → error naming the key. Below 1.0 the cap falls *below* the
  ceiling, so the rule would drop legitimate **sub-ceiling** ranges — values
  that today interpolate to a real score between 0 and 100 — which is a
  footgun disguised as configuration, not a narrowing of an existing
  saturation.

The key accepts an integer or a float literal (`2` and `2.0` both mean 2.0).
TOML integers do not coerce to `f64` through serde, and an operator typing
`2` should not get a type error instead of a cap; the config deserializer
accepts either form and the tests pin both.

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

### 6. Observability: the breakdown is the operator-visible signal

Two layers, ordered by how reliably the operator sees them:

- **Always visible — the breakdown string.** `list`, `show` and the review
  card render it with no configuration at all, so the operator sees why a
  score is shaped the way it is. This is the signal the spec promises.
- **Log — one `warn!` per refused range**, with identifying fields
  (`company`, `title`, `min`, `max`, `cap`, `multiplier`). Expected frequency
  is low (5 per 100 leads on the worst day measured, none on most), which
  makes `warn` signal rather than the wallpaper an `info` would become.

Honest caveat, found in review and verified in code: with telemetry disabled
(the default, and this operator's setting) the file sink's filter floor is
`error` (`env_filter`, decision 0005), so **no crate `warn!` reaches the log**
unless the level is raised. PR #47 rescued the warn floor only on the
telemetry-on path (`telemetry_env_filter`), so the off path is
silence-by-construction for exactly this class of message. Changing that is a
cross-cutting logging-policy decision, not a scoring rule, so this change does
not touch it and the spec does not promise the warn. Filed as follow-up
(GWLJ-5ca8pg).

Deliberately **not** added:

- a discovery-summary or discovery-event counter — plumbing a score outcome
  back into the discovery payload for a rare event; the breakdown plus the
  `rescore` changed-count cover the operational need for now;
- a span on `score()` — it is a pure function that performs no I/O, and this
  change introduces none; the 0011 floor applies to boundaries, not to pure
  computation;
- promoting the existing `theirstack_mismatch` comp span (currently `debug!`)
  — that signal answers a different question (structured field vs text-quoted
  comp) and fires on ordinary disagreements; recorded as a follow-up on
  GWLJ-f5pdka.

### 7. When only the top of the range is junk, the dimension still drops

The compared value is `max.or(min)`, so a range like `min 180,000 / max
187,200,000` is implausible as a whole and the dimension drops, even though
the bottom looks sane. Three alternatives were weighed:

- **Score against `min` when `max` is implausible.** Systematically
  *understates* — `min` scores at or near 0 against a floor above it — and
  pretends the posting pays its bottom of range.
- **Clamp `max` to the cap.** Systematically *inflates*: a clamped value
  lands at or above the ceiling and scores 100, which is the bug being fixed.
- **Trust the bound that survives and flag the other.** More machinery than
  the signal warrants, and it invents a comparison rule the floor gate does
  not share.

Dropping is the only option that adds no information the data does not
support: we know the range is untrustworthy, not where in it the job sits.
All five records measured on 2026-10-28 have *both* bounds implausible, so the
practical cost today is zero.

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

## Follow-ups (filed, not attempted here)

- `GWLJ-5ca8pg` — the crate's `warn!` floor with telemetry off: the file sink
  filters every crate warn at the default level, so this change's warn (and
  every other operational warn) is invisible unless the level is raised.
- Annualize from TheirStack's source-published `salary_currency` /
  `salary_period` / `min_salary` / `max_salary` instead of trusting the
  feed's annualized USD fields (~2.5% fill, jobs since 2026-10 only).
- Promote the `theirstack_mismatch` comp signal out of `debug!` once we know
  how noisy it is.

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
- A gate test asserting an above-cap range produces **no** gate failures (the
  spec's "never rejected, down-gated, or excluded" clause needs a net).
- Config tests for the multiplier in both literal forms (`2` and `2.0`) and
  for the rejected values.
- Config validation tests: `NaN`, `inf`, `0.5`, and an accepted `3.0`.
- Property tests: (a) any value above the cap produces the same result as
  `comp: None`; (b) within `[floor, cap]` the score is monotone
  non-decreasing in value; (c) `normalized(min>max)` equals
  `normalized(max>min)`.
- An integration test that ingests a fixture with an implausible range,
  rescores it, and asserts the recorded score loses the dimension.
