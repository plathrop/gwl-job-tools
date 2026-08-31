# 0010: Lifecycle status is derived; `list` shows the active pipeline

Status: accepted 2026-08-31

## Context

`gwl-jobs list --all` rendered marks and outcomes as coequal bracket
tags (`[apply-manual] [applied]`), and the default `list` printed only
the pending review queue — duplicating what `review` already shows and
saying nothing about the pipeline the user actually watches: what's
pending, what's being applied to, what's been applied, and which of
those were automated. Two leads in the wild made the conflation visible:
one recorded the `applied` fact with no decision mark, one recorded the
`apply-manual` decision with no `applied` fact — and both looked like
alternative "statuses" rather than a decision and a fact.

## Decision

- **Status is a derived view, not a stored dimension.** The events stay
  as they are: `reviewed` records a decision, `applied` records a fact,
  and making review's `m` key emit `applied` would fabricate an
  execution at decision time. `LeadRecord::lifecycle_status()` computes
  the single stage dimension from the facts (design doc 0002 has the
  rule table); the latest outcome wins, and the method for `applied`
  prefers the recorded fact, falling back to the mark's implication.
- **`applied --method` defaults from the lead's mark.** After a review
  mark, `gwl-jobs applied <lead>` alone is enough; an explicit
  `--method` still wins. This ends the double entry of the automation
  bit (it previously lived in both the mark and the flag, and could be
  absent from both).
- **`list` (default) prints the active pipeline**: every lead that is
  neither terminal nor durably ignored, ranked by score. `--all` adds
  terminal and ignored leads back. The pending queue is unchanged and
  remains what `review` steps through. **Gate-rejected leads are
  included by design**: a machine rejection is not a terminal state,
  they sort to the bottom with a `[rejected]` tag, and they are
  `edit`-revivable (correcting a mis-extracted remote signal, or a comp
  that now passes the floor, restores them to the queue). If that
  proves noisy in practice, excluding them is a one-line change.
- **Ignored leads are excluded from the default view** (settled with
  Grey, 2026-08-31): the ignore mark exists to bury leads permanently —
  durable-ignore means out of sight, and `--all` is the only view that
  reveals them.

## Rationale

- The user-facing question is one-dimensional ("what stage is this at,
  and was it automated?"); two parallel latest-wins fields answer it
  ambiguously.
- Rendering one derived tag costs nothing at the event layer — the log
  keeps full decision/fact provenance for `show --json` and `events`.
- Excluding only terminal outcomes from the default would resurrect
  deliberately buried leads into the pipeline view, defeating the point
  of the ignore mark.

## Consequences

- `render.rs` shows one Status row/tag; mark and outcome rows are gone
  from the card (facts remain in `show --json` / `events`).
- Review-loop `a`/`m` keys print a one-line reminder to record
  `applied` (and the `m` path points at `show --jd`) so leads don't
  linger in `applying (…)` forever.
- `OutcomeView` gains the `method` field (projection-only, additive).
- Design doc 0002 carries the rule table; 0001 §5/§7/§8 amended by
  pointer.
