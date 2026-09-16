## Context

Two paths replay the event log into state, and they disagree on malformed
payloads:

- `Projection::rebuild` (`src/projections/mod.rs`) — the read model — already
  decodes every known event type strictly: `serde_json::from_value::<T>(...)
  .into_diagnostic().wrap_err(...)?` hard-errors with the event's id and seq.
- `lead::evolve` (`src/domain/lead.rs`) — the aggregate decide path, reached
  via `commands::replay_lead` — is lenient: `if let Ok(...)` swallows
  `SnapshotFields`/`ScoredPayload` decode failures, and sets `state.exists =
  true` *before* the decode, leaving a lead that "exists" with no snapshot.

See proposal.md for the motivation.

## Goals / Non-Goals

**Goals:**

- Make `lead::evolve` fail loudly on malformed payloads, matching the
  projection's existing strict contract.
- Leave no partial aggregate state behind on a decode failure.

**Non-Goals:**

- No upcaster: the corpus was verified clean (oldest event is one day after
  the source→adapter rename), so no legacy payloads exist to migrate.
- No change to the projection (`rebuild` is already strict).
- No change to unknown-event-type handling (forward compatibility).

## Decisions

### 1. Make `evolve` return `Result`, hard-error on decode failure

`evolve` changes from `fn evolve(state, event)` to
`fn evolve(state, event) -> Result<()>`. Each known event type decodes its
payload with the same strict pattern the projection uses
(`serde_json::from_value::<T>().into_diagnostic().wrap_err(…)`), propagating
the error instead of `if let Ok`.

**Why:** the codebase's contract is already "corruption is a hard error"
(jsonl.rs refuses a corrupt log; `rebuild` refuses malformed payloads).
`evolve` is the one lenient holdout. Consistency demands it match.

**Alternatives considered:** warn-and-skip ("counted") was rejected — it
would still leave `state.exists = true` with no snapshot (partial state), and
it would diverge from `rebuild`, so a lead could present one way in the
projection and another in the decide path.

### 2. Decode before mutating state

Reorder the snapshot arm so the `SnapshotFields` decode happens first and
`?`-propagates on failure; only then set `exists`/`eval_revision`/snapshot
fields. This keeps `evolve` side-effect-free on error even though the caller
discards the state when replay bails.

### 3. Reuse the same typed payload structs as the projection

Decode into `SnapshotFields`, `ScoredPayload`, and `ReviewedPayload` — the
same structs `rebuild` uses — rather than hand-rolled field extraction. This
prevents the two paths from drifting.

The `reviewed` arm currently reads `mark` with
`.get("mark").and_then(Value::as_str)`; it becomes a strict `ReviewedPayload`
decode. This is the lowest-severity case (a missing mark means "unmarked,"
not data loss), but it is included so `evolve` and `rebuild` are uniform.

### 4. Keep unknown event types ignored

The `_ => {}` arm is unchanged: an unrecognized `type` is forward-compatible
data, not corruption. This is a spec requirement (see
`specs/event-replay/spec.md`).

## Risks / Trade-offs

- [A corrupt log now hard-fails the decide path too] → Mitigation: `rebuild`
  already hard-fails the same corruption first, so this cannot newly brick a
  command that would otherwise have succeeded; it only removes the silent
  degradation that was already unreachable in practice.
- [Signature change ripples to callers/tests] → Mitigation: the only
  production caller is `commands::replay_lead`, which already returns
  `Result` and can `?`-propagate. Test call sites update mechanically.
- [Strict `reviewed` decode rejects a legacy reviewed event] → Mitigation:
  `decide_mark` always writes a string `mark`; the corpus has no malformed
  reviewed events.

## Migration Plan

Pure code change — no data migration. The corpus needs no rewrite (verified
clean). Rollback is a revert of the commit.
