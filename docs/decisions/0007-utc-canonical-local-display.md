# 0007: UTC canonical timestamps, local-timezone display

Status: accepted 2026-08-28

## Context

The event log stores `occurred_at` and `recorded_at` as RFC 3339 UTC
timestamps (e.g. `2026-08-28T19:32:19Z`). As the tool grows UI-style output
(the review queue, `list`, `show`), timestamps will be rendered for a human.

## Decision

- **Canonical storage is UTC** — the event log and all machine-readable
  output (JSON) keep RFC 3339 UTC timestamps.
- **Display is local timezone** — human-facing output renders timestamps in
  the user's local timezone (via `jiff`'s `TimeZone::system()`).

## Rationale

- UTC is unambiguous, sortable, and timezone-independent — the right
  canonical form for a source-of-truth log.
- Humans read dates in their own timezone; a UTC timestamp on a review card
  is a small but real friction (the project's energy-economics goal).

## Consequences

- The event log and JSON output are unchanged (already UTC).
- When UI-style output lands (Increment 4's `list`/`review`), timestamps are
  rendered via `jiff::Zoned` in the system timezone.
