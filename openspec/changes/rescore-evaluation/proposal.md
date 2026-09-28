# Proposal

## Why

Evaluations (gates + score) are computed against the config at ingest
time and the result is baked into the append-only event log — with no
way to re-run them when the config changes, is corrected, or was
entirely missing. On 2026-09-28 a missing config file caused a batch of
ingests to evaluate against `Config::default()`, and the only recovery
would have been re-ingesting every lead. The tool needs a command that
re-runs the evaluation over the stored snapshot with the *current*
config — the batch counterpart of the re-evaluation the `edit` path
already performs.

## What Changes

- **`gwl-jobs rescore <lead>`**: re-run gates + scoring for one lead,
  using the current config against the lead's latest stored snapshot
  (`raw_text` is persisted, so no refetch). Appends `rejected` XOR
  `scored` events with `revision + 1` — no new event type; decision
  0006's "`scored` is the pass-marker" invariant carries over unchanged.
  A gate-rejected lead can revive (a corrected config passes it back
  into the pipeline); a previously-scored lead can newly reject.
- **`gwl-jobs rescore --all`**: the same re-evaluation over the visible
  universe — every non-terminal, non-ignored lead. Gate-rejected leads
  are in scope (that is the revival case); ignored leads are excluded
  (scoring a buried lead is invisible work); terminal leads are excluded
  (their score is a historical fact). Marks and outcomes are untouched:
  a rescore appends evaluation events only.
- **Idempotent runs still append**: a re-evaluation that changes nothing
  is still a fact that happened; the log records it. The command reports
  the delta: N evaluated, M score changes, K newly rejected, J revived.
- **`gwl-jobs rescore --dry-run`**: fetches nothing and appends
  nothing — evaluates against the current config in memory and prints
  the would-change summary (which leads change score, become rejected,
  or revive), the same contract as `discover --dry-run`.
- **Observability (decision 0011)**: a span per rescore invocation with
  evaluated/changed/rejected/revived counts; per-lead `debug` events
  with old/new composite; an `info` summary line matching the command
  output; the same counts in `--json` output.

Out of scope (filed as pebble GWLJ-04u65w, referenced not included):
the root-cause fix for silent-default config evaluation.

## Capabilities

### New Capabilities

- `rescore`: re-running the gate + score evaluation over stored
  snapshots with the current config — single-lead and batch forms,
  dry-run preview, and the event/revision contract the re-evaluation
  must satisfy.

### Modified Capabilities

None. `discovery` and `event-replay` are untouched; the change appends
existing event types and reads existing projection state.

## Impact

- **Code**: `src/domain/lead.rs` (a `decide_reevaluate` decide path —
  the aggregate counterpart of `decide_edit`'s evaluation, minus the
  snapshot event), `src/commands/rescore.rs` (new), `src/cli.rs`
  (command wiring into the Triage group), `src/event_store` (nothing —
  appends only), projections (nothing — `rejected`/`scored` already
  resolve latest-wins).
- **Dependencies**: none new.
- **No breaking changes**: no event-schema or existing-command changes;
  the new command only appends existing event types.
- **Docs**: README command table; design doc 0001 §3 gains the rescore
  evaluation rule alongside edit's (both are re-evaluations over the
  stored snapshot; the difference is provenance: `edited` events carry
  user corrections, rescore events carry no snapshot).
