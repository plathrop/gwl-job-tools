# 0002: v0 event, mark, and command naming

Status: accepted 2026-08-25 (design review of PR #3, Increment 0 /
GWLJ-rk2cnb)

## Context

The Increment 0 design draft named event types with a `lead_` prefix
(`lead_ingested`, `lead_reviewed`, …), included a `no-action` review mark,
and named the queue-listing command `gwl-jobs queue`. Review renamed all
three. Because these names propagate into the spec, the design doc, pebble
descriptions, and every future event in the log, the vocabulary is recorded
here so it is not re-litigated piecemeal.

## Decision

- **Event types carry no aggregate prefix:** `ingested`, `updated`,
  `rejected`, `scored`, `reviewed`, `apply_queued`, `reingest_suppressed`,
  and the outcome set (`applied`, `screened`, `interviewed`, `offered`,
  `accepted`, `rejected_by_employer`, `withdrawn`, `declined`, `archived`).
  The `lead/<id>` stream prefix already namespaces them. If a second
  aggregate ever appears, namespace the `type` string then — not before.
- **The review mark is `defer`** (not `no-action`), matching the "deferral
  count" terminology used everywhere else.
- **The queue-listing command is `gwl-jobs list`** (not `queue`). "Review
  queue" remains the name of the concept; `list` is the verb.

## Rationale

- The `lead_` prefix was semantically null: every event lives on a
  `lead/<id>` stream and v0 has exactly one aggregate.
- "no-action" read as "do nothing," but the mark increments a deferral
  count and re-queues the lead — `defer` says what it does.
- `list` is the conventional CLI verb, and `gwl-jobs list --all` reads
  cleanly.

## Consequences

- The spec and design doc 0001 use the new vocabulary; the increment pebble
  descriptions (GWLJ-g8gbo3, -2hrdit, -1hp1r6, -1lxx3e, -x39zmv) were
  updated to match.
- No migration burden: these names land as `schema_version: 1` before any
  real event is written, so no upcasters are needed for the rename.
