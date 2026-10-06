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
existing helpers, unchanged. A gate-passing re-evaluation appends one
`scored`; a failing one appends one `rejected` per failed gate (that is
`rejection_events`' existing shape).

**The pass-marker invariant needs a projection fix, not just a claim.**
Decision 0006 (`scored` is the pass-marker) reads "latest evaluation
wins", but the projection implements that only *implicitly*: the
snapshot handler clears both `latest_rejection` and `latest_score`, so
an evaluation event arriving after a snapshot lands on cleared state.
Evaluation events set their own field and do **not** clear the
counterpart. Today every evaluation follows a snapshot in the same
batch, so the discipline holds — but `decide_reevaluate` appends
evaluation events with *no* snapshot, and without a fix the
newly-rejected direction (the case this change exists for) breaks: a
lead with a stale `latest_score` that re-evaluates into `rejected` would
keep the stale score, read `is_gate_rejected() == false`, and linger in
the default list, mis-tagged, with a ghost score. The fix makes
latest-wins explicit at the projection layer: the `REJECTED` handler
clears `latest_score`, the `SCORED` handler clears `latest_rejection`.
With that, both directions are sound (a revived lead's stale rejection
is cleared too, so no consumer reading `latest_rejection` directly
sees a ghost), and `is_gate_rejected()` (decision 0013) stays correct
under the new no-snapshot discipline.

**The revision counter needs the aggregate-side counterpart.** `LeadState::eval_revision` counts snapshot events (an evaluation failing multiple gates emits several `rejected` events at one revision, so counting events would over-count), which means a no-snapshot rescore would not advance it on replay — a second rescore would reuse the revision. `evolve` therefore takes the payload's `revision` as a floor on `rejected`/`scored` (snapshot events keep it exact), and both directions are tested.

*Alternative*: reuse `decide_edit` with a no-op field diff — rejected
because the `edited` event would fabricate user provenance, and the
"nothing to edit" bail exists precisely to keep edits honest.
*Alternative considered*: emitting a snapshot event alongside the
evaluation (keeping today's discipline intact) — rejected because it
would fabricate a re-ingest provenance and double the log growth for
no information gain.

### 2. Single-lead form works on any lead; batch is the visible universe

`rescore <lead>` is an explicit operator action on a named lead — any
state, including ignored and terminal (mirrors `edit`, which also does
not suppress ignored leads). `rescore --all` is the sweep, and its
scope is the complement of decision 0013's exclusion minus the
gate-rejected clause: non-terminal ∧ non-ignored leads, **gate-rejected
included** (the revival case). Note this is *not* the `list` default
view (which after DR 0013 hides gate-rejected) — the batch iterator is
`ranked_leads()` filtered `!is_terminal() && !is_buried()`, not
`active_leads()`. Rescoring terminal leads in bulk would rewrite
history; rescoring ignored leads in bulk is work nobody can see.

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
