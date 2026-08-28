# 0006: `scored` is the pass-marker

Status: accepted 2026-08-28 (Increment 3 / GWLJ-1hp1r6)

## Context

Design doc 0001 §1 flags a known limit of the batch-prefix crash policy: if a
snapshot event (`ingested`/`updated`) persists but its following `rejected`
event is torn away by a crash mid-batch, the lead presents as gate-passing
until the next re-ingest. The design doc deferred the fix to the scoring
increment, with `scored` as the candidate marker (GWLJ-8vshug).

## Options

- **Add an explicit `evaluated` pass-marker event** — a dedicated event
  recording that an evaluation passed gates.
- **Treat `scored` as the marker** — a lead whose latest evaluation passed
  gates already carries a `scored` event; its absence (with a snapshot
  present) means the evaluation did not pass.
- **Accept the edge** — leave the torn-rejection window as-is.

## Decision

`scored` is the pass-marker. No new event type.

## Rationale

- `scored` is emitted exactly when an evaluation passes gates, in the same
  batch as the snapshot event. A lead with a snapshot but no `scored` (and no
  `rejected`) is precisely the torn-tail case — the projection can treat it as
  "not passed" rather than "passed."
- An explicit `evaluated` event would duplicate `scored`'s role (every
  gate-passing evaluation already emits `scored`), adding a second event per
  ingest for no new information.

## Consequences

- The projection's queue-membership rule (design doc 0001 §7) keys on
  "current `scored`, no subsequent failing `rejected`" — which is exactly the
  pass-marker semantics.
- GWLJ-8vshug is closed with this record.
