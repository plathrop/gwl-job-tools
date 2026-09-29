# Design

## Context

Evaluation (gates + score) happens at ingest and edit time against the
config loaded for that invocation, and the resulting events are baked
into the log. The `edit` path already re-evaluates with the current
config over the stored snapshot (`commands/edit.rs` runs
`gates::evaluate` + `scoring::score`, then `decide_edit` appends
`rejected` XOR `scored`) — that is how gate-rejected leads are
edit-revivable. What is missing is the same re-evaluation without a
field correction, and in batch. See proposal.md — Why, for the incident
that motivated this.

Everything the evaluation needs is already persisted: `raw_text` lives
in every snapshot event, and `LeadState` carries the evaluation revision.

## Goals / Non-Goals

**Goals:**

- One decide path that re-evaluates a stored snapshot with the current
  config, appending evaluation events only.
- Single-lead and batch forms, plus a no-write dry-run preview.
- Observability sufficient to answer "did it work, and how much
  changed?" (decision 0011).

**Non-Goals:**

- Re-extraction or refetching — the stored snapshot is the input.
- Touching marks or outcomes — a rescore never changes review state.
- Config-load signaling on defaults (pebble GWLJ-04u65w, separate).
- The `discover`/`ingest` paths — unchanged.

## Decisions

### 1. A dedicated `decide_reevaluate`, not a repurposed `decide_edit`

`decide_edit` bails when no field changed, and emits an `edited` event
whose `adapter: "user"` provenance asserts a user correction — both
wrong for rescore. The new decide path takes the replayed `LeadState`
plus fresh `gate_failures`/`score` computed by the command layer, and
appends `rejection_events` / `scored_event` with `revision + 1` — the
existing helpers, unchanged. The pass-marker invariant (decision 0006:
`scored` present ⟺ latest evaluation passed) holds by construction: a
gate-passing re-evaluation appends `scored`, a failing one appends
`rejected`, and the projection's snapshot-event handler already clears
stale rejection/score state — evaluation events resolve latest-wins.

*Alternative*: reuse `decide_edit` with a no-op field diff — rejected
because the `edited` event would fabricate user provenance, and the
"nothing to edit" bail exists precisely to keep edits honest.

### 2. Single-lead form works on any lead; batch is the visible universe

`rescore <lead>` is an explicit operator action on a named lead — any
state, including ignored and terminal (mirrors `edit`, which also does
not suppress ignored leads). `rescore --all` is the sweep, and its
scope is the same universe `list` shows: non-terminal ∧ non-ignored,
gate-rejected included (the revival case). Rescoring terminal leads in
bulk would rewrite history; rescoring ignored leads in bulk is work
nobody can see.

### 3. Always append; report the delta, don't suppress no-ops

A re-evaluation that changes nothing is still a fact that happened.
Suppression would make the log silent about the operator's action and
make "did the config fix land?" unanswerable from history. The command
output and spans carry the delta counts (evaluated / changed / newly
rejected / revived / unchanged), so a no-op run is visible as one.

### 4. Dry-run needs no store machinery at all

`discover --dry-run` needs a `MemStore` because the decide loop appends.
A rescore dry-run appends nothing, so it never opens the writer: build
the projection from `read_only_envelopes` (the lock-free path shipped
with `discover --dry-run`), evaluate each lead in memory, and print.
The summary must be computed by the same evaluation code as the real
run so the preview cannot drift (shared helper, `dry_run` flag on the
command, not a fork of the logic).

### 5. Batch holds the single-writer lock for the whole run

One `open_workspace` for the run; appends go through the existing
store discipline per lead, re-projecting only the touched lead (the
pattern `ingest_url` uses). The lock is expected — `rescore --all` is
operator-invoked maintenance, not concurrent machinery.

### 6. Observability (decision 0011 test: what does an operator need?)

To answer "did this work, and how well?": a `rescore` span per
invocation with `evaluated`, `changed`, `rejected`, `revived` fields;
per-lead `debug` records with old/new composite and lead id; an `info`
summary line identical to the command output; `--json` output carrying
the same counts for scripting. The dry-run emits the same span shape
with `dry_run = true`, so Honeycomb can compare preview vs. real run.

## Risks / Trade-offs

- [Batch log growth: `--all` appends one event per lead] → It is
  operator-invoked and reported; dry-run exists to preview first. Log
  growth is bounded by corpus size per run, not by time.
- [A revived lead re-enters the pending review queue with a stale mark]
  → Marks are untouched by design (a rescore is not a review decision);
  the pending queue's mark rules already handle re-entering leads the
  same way `edit` revivals do.
- [Config semantics drift between the ingest that stored a lead and the
  config at rescore time] → That is the feature, not a risk, but the
  score breakdown (`Score: N = …` line) must render from the current
  evaluation so the operator can see *why* it moved.

## Migration Plan

Additive: a new command appending existing event types. No upcasting, no
schema version. Rollback is "don't run it"; appended events are ordinary
evaluation events the projection already resolves.
