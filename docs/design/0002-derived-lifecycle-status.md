# 0002: Derived Lifecycle Status and the `list` Default

Status: accepted (Increment, GWLJ-w27khs)
Spec: `docs/specs/job-search-automation-v2.md`
Amends: design doc 0001 §5, §7, §8; decision record 0010

## The problem

Marks and outcomes are distinct _events_ — a decision (`reviewed
{apply-manual}`) vs. a fact (`applied`) — but the projection exposed them
as two parallel, independent latest-wins fields, and `list`/the card
rendered both as undifferentiated bracket tags:

```
… [apply-manual] [applied] …
```

Read as coequal "statuses", this conflates state dimensions: a user who
cares about one thing — _what stage is this application at, and was it
automated?_ — had to mentally merge two fields that can also disagree
(the "automated?" bit lives in both the mark and `applied --method`, and
both can be absent).

## Decision (decision record 0010)

**Status is a derived view, not a stored dimension.** The events stay
exactly as they are — marks record decisions, outcomes record facts, and
merging them (e.g. review's `m` emitting `applied`) would fabricate an
execution at decision time. Instead, `LeadRecord::lifecycle_status()`
computes the single application-stage dimension from the facts:

| Facts present                                   | Status                            |
| ----------------------------------------------- | --------------------------------- |
| outcome `applied`, method known (fact or mark)  | `applied (manual\|auto-assisted)` |
| outcome `applied`, method unknown               | `applied`                         |
| other outcome (`screened`, `offered`, terminal) | that outcome's name               |
| mark `apply-automatically`, no outcome          | `applying (auto-assisted)`        |
| mark `apply-manual`, no outcome                 | `applying (manual)`               |
| mark `defer`                                    | `deferred`                        |
| mark `ignore`                                   | `ignored`                         |
| no mark, standing score                         | `pending`                         |
| no mark, standing rejection                     | `rejected (gate)`                 |
| neither (torn-batch edge)                       | `ingested`                        |

Rules:

- **The latest outcome (fact) wins** over the mark (decision): an outcome
  is the later stage.
- **The method prefers the recorded fact, then the mark's implication**
  (`apply-automatically` → auto-assisted, `apply-manual` → manual). This
  also removes double entry: `gwl-jobs applied <lead>` with no `--method`
  defaults from the lead's mark; an explicit `--method` still wins.
- `list` lines and the lead card render **one** status tag/row. The
  underlying mark/outcome facts remain visible via `show --json` and
  `events`.

## `list` default: the active pipeline

`list` previously printed only the pending review queue (§7), duplicating
the first thing `review` does. Now:

- **`gwl-jobs list`** (default) prints the **active pipeline**: every
  lead that has neither reached a terminal state (`accepted`,
  `rejected_by_employer`, `withdrawn`, `declined`, `unresponsive`,
  `archived`) nor been durably ignored, ranked by composite score
  descending, first-seen as the tie-breaker — the same ranking as
  `--all`.
- **`gwl-jobs list --all`** adds terminal and ignored leads back in
  (unchanged set: all leads).
- The **pending review queue is unchanged** and remains what `review`
  steps through (§7); it is a subset of the active pipeline.

Notes:

- `is_terminal` is latest-wins on `occurred_at`: a later non-terminal
  outcome (e.g. `applied` recorded after `archived`) un-terminals the
  lead.
- **Ignored leads are excluded from the default view** (settled with
  Grey, 2026-08-31): the ignore mark exists to bury leads permanently,
  and durable-ignore means out of sight — `list --all` is the only view
  that reveals them. Excluded-set = terminal outcomes ∪ ignored marks.

## Review-loop hints

The decision and the execution are still two steps by design — the
final click is the user's — so both apply paths now end with a one-line
reminder to record the fact (`gwl-jobs applied <prefix>`), and the
`m` path also points at the JD (`gwl-jobs show <prefix> --jd`). This
keeps leads from lingering in `applying (…)` forever because the user
forgot the second command.
