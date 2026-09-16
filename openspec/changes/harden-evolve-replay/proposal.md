## Why

`lead::evolve` — the aggregate's replay path that builds `LeadState` for
`decide_ingest`/`decide_edit` — silently swallows payload deserialization
failures with `if let Ok(...)`, and sets `state.exists = true` *before* the
decode. A lead whose snapshot event carries a legacy or malformed payload
ends up "existing" with no snapshot/adapter/url/raw_text. The read-model
projection (`Projection::rebuild`) already treats the same condition as
source-of-truth corruption and hard-errors. This closes that inconsistency
and the latent data-integrity bug.

## What Changes

- `lead::evolve` returns `miette::Result<()>` and hard-errors — identifying
  the event — when a *known* event type's payload fails to deserialize,
  instead of silently skipping:
  - `ingested`/`updated`/`edited` → `SnapshotFields`
  - `scored` → `ScoredPayload`
  - `reviewed` → `ReviewedPayload` (a missing/non-string `mark` becomes an
    error, matching the projection)
- `commands::replay_lead` propagates the new error with `?`.
- `Projection::rebuild`'s `reingest_suppressed` arm is hardened: it was the
  one remaining lenient decode, silently skipping a malformed payload; it is
  now a strict `SuppressedView` decode with a regression test.
- Unknown event types remain ignored (forward compatibility) — unchanged.
- No upcaster is added: the live corpus was verified clean (oldest event is
  one day after the source→adapter rename), so there are no legacy payloads
  to migrate.

## Capabilities

### New Capabilities

- `event-replay`: the contract that replaying a *known* event type whose
  payload fails to deserialize is a hard error (source-of-truth corruption),
  never a silent skip or partial state.

### Modified Capabilities

<!-- none — openspec/specs/ is empty; this is the first capability -->

## Impact

- `src/domain/lead.rs` — `evolve` signature and per-event-type decode paths.
- `src/commands/mod.rs` — `replay_lead` propagates the new error.
- `src/projections/mod.rs` — `reingest_suppressed` arm hardened from `if let
  Ok` to a strict decode.
- `src/domain/lead.rs` tests — update `evolve` call sites; add malformed-
  payload hard-error tests (legacy `source`-without-`adapter` snapshot,
  malformed `scored`, malformed `reviewed`).
- `src/projections/mod.rs` tests — add a malformed `reingest_suppressed`
  hard-error test.

Behavior note: a log that previously degraded silently now refuses to replay.
That is the intent — and already the projection's behavior — so a corrupt
snapshot can no longer mint a lead that "exists" with no data.

## Observability

No new instrumentation. The new failure mode — a malformed payload refusing
to replay — is itself the instrumentation: a hard `miette` error naming the
event's type, id, and seq, surfaced through the enclosing command span.
`evolve` is a pure in-memory decode invoked per event in the replay loop, so
a per-event span would be noise, not signal; the boundary to instrument
(were any needed) is `replay_lead`, not `evolve`.
