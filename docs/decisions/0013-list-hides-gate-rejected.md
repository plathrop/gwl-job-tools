# 0013: `list` hides gate-rejected leads by default

Status: accepted 2026-09-28 (supersedes the gate-rejected clause of
decision record 0010; daily-use evidence)

## Context

Decision 0010 chose to include gate-rejected leads in `list`'s default
"active pipeline" view: a machine rejection is not a terminal state, the
leads sort to the bottom with a `[rejected (gate)]` tag, and they stay
`edit`-revivable. The same record predicted the failure mode: "If that
proves noisy in practice, excluding them is a one-line change."

Four weeks of daily use proved it noisy. A single `discover` run over
the free feeds can reject most of a batch, and the rejected block —
leads the operator has, by definition, no action to take on — lengthens
the default listing every single run, pushing the scored queue that is
the point of the view further down the screen.

## Options

- **A `--hide-gate-rejected` flag** — keeps 0010's default intact, but
  the operator types it every single run. A flag used 100% of the time
  is a default wearing a costume.
- **A config option** (`[list] hide_gate_rejected = true`) — same daily
  effect, one more config knob to carry, and a fresh user silently gets
  the noisy behavior 0010's evidence has already retired.
- **Flip the default** — 0010's own pre-authorized reversal, once
  evidence arrived. The evidence has arrived.

## Decision

`list`'s default view excludes gate-rejected leads (a lead whose latest
evaluation failed a gate), alongside terminal and durably ignored
leads. `list --all` remains the union view that reveals all three
classes.

Gate-rejected leads are unchanged in every other respect: they remain
`edit`-revivable (a corrected snapshot re-evaluates and restores the
lead to the default view when it passes), `show` and `events` still
expose them, and the `[rejected (gate)]` tag still distinguishes the
machine's content filter from the user-recorded
`rejected_by_employer` terminal outcome. The pending review queue
(`review`) is untouched — it already excluded gate-rejected leads
(scores are its membership rule).

## Rationale

- The default view exists to answer "what should I look at?" — the
  scored, unjudged queue. A gate rejection is a machine's settled
  answer of "no"; showing it in that view is re-litigating a decision
  nothing new has arrived to change.
- Revival needs no visibility: `edit` operates by lead id, discover and
  ingest report rejections at the moment they happen, and `--all`
  exists for the audit view.
- Reversal is cheap and safe: the exclusion is one predicate in
  `Projection::active_leads()`, covered by the same ranking and tag
  machinery; `--all` is unchanged.

## Consequences

- `Projection::active_leads()` gains a third exclusion
  (`is_gate_rejected`, mirroring the `lifecycle_status` condition:
  standing rejection, no score since).
- `--all`'s help text and the "list default" section of design doc 0002
  now describe terminal ∪ ignored ∪ gate-rejected as the excluded set.
- A torn-batch lead (snapshot present, evaluation event missing) still
  appears in the default view with status `ingested` — decision 0006's
  crash-window semantics are unchanged; only a standing `rejected`
  excludes.
