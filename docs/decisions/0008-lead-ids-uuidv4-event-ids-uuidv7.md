# 0008: Lead IDs are UUIDv4; event IDs stay UUIDv7

Status: accepted 2026-08-29

## Context

`lead_id` was minted with `Uuid::now_v7()`, the same scheme as event IDs.
UUIDv7's first 48 bits are a millisecond timestamp, so leads ingested in the
same session share their first ~8–12 hex characters _by construction_. The
`<lead>` addressing scheme (design doc 0001 §8) is "unambiguous UUID prefix",
so batch ingest — the tool's core workflow — routinely makes short prefixes
ambiguous: `mark 01a04d4b defer` fails with "matches 2 leads" when two leads
were minted in the same second. (Found by Remi during PR #10 review.)

## Decision

- **`lead_id` is UUIDv4** — fully random, so short prefixes are unambiguous
  with overwhelming probability (6 hex chars = 24 bits; ~0.03% collision
  odds at 100 leads).
- **Event IDs stay UUIDv7** — the event log is append-only and time-ordered;
  v7 gives sortable, monotonic IDs for free, and its index locality helps if
  the SQLite projection ever lands.

## Rationale

- A lead's creation order is already recorded better in the event log (the
  first `ingested` event's `recorded_at`), so the lead ID does not need to
  carry time. The only thing v7 buys a lead — "sort by ID = creation order"
  — is redundant with the projection's `first_seen`.
- Leads are human-addressed handles; events are log entries. Each should use
  the scheme that fits its use: random for addressability, time-ordered for
  append locality.
- The deliberate inconsistency (v4 leads, v7 events) is the point, not a
  compromise — the two identifiers serve different purposes.

## Consequences

- `record_ingest` mints `Uuid::new_v4()` for new leads; event and
  correlation IDs are unchanged (v7).
- Design doc 0001 §2 updated to say `lead_id` is UUIDv4.
- Adaptive prefix length (extend until unambiguous, git-style) remains a
  possible future complement, but v4 makes the collision astronomically rare
  in the first place.
