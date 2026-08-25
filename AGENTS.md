# AGENTS.md

Guidance for AI agents (and humans) working on `gwl-job-tools`.

## Project Overview

`gwl-job-tools` is a CLI job-search triage and application-assist tool. It
automates the high-volume, low-judgment work of a job search (sift, score,
capture metadata, prepare application materials) and reserves human judgment
for the high-value decisions (which jobs to pursue).

The full specification is **`docs/specs/job-search-automation-v2.md`**.
Read it before starting work. This file carries the always-relevant context;
the spec carries the scope.

## Non-Negotiable Guardrails

These come from the spec and apply to every change:

- Never auto-submit an application without explicit per-job approval. The
  "apply-automatically" mark _is_ that approval; the final browser click is
  expected and fine.
- Never fabricate or embellish experience or skills.
- Never contact people (recruiters, insiders) automatically.
- Be a good citizen: respect ToS and rate limits (200–500ms delay between
  requests), obey control headers and redirects.
- Never send resume data to a service the user hasn't approved (moot in v0 —
  no LLM/API calls).
- Never write a cover letter from scratch; use the human-written corpus as the
  style guide (future constraint — v0 uses a generic letter).
- Never match blacklisted companies.

## Key Decisions (settled — do not re-litigate)

- Rust for the v0 core; Playwright (Python) shell-out is vNext.
- v0 is fully deterministic — no LLM, no API keys, no non-determinism.
- Event-sourcing: append-only JSONL event log is the source of truth; in-memory
  projection for reads; SQLite deferred.
- Extend this crate in place; redesign the model for richer data.
- Scoring = hard filters (gates) + weighted-sum composite; 0–100 with a
  confidence field.
- Unknown/missing comp passes the floor gate; the comp dimension drops out of
  the composite with weight renormalization.
- "apply-automatically" mark = the approval; no second confirmation.
- Cover letters: generic letter for auto-applications; manual for others; no
  generation in v0.
- No age/deadline weighting.
- Config = TOML.

## Delivery Workflow

- Deliver in small increments, each as a PR. PRs are the review checkpoint.
- When opening a PR, add @remi-ashe as a reviewer.
- The agent may continue ahead via stacked PRs, capped at 2.
- Before opening a new stacked PR, check for review feedback (Copilot, Remi,
  human) on existing PRs and incorporate it.
- The first increment is a design PR (event schema + command surface) that must
  answer: lead identity & re-ingest semantics, full lifecycle events, alpha CSV
  fate, and the review-loop interaction model. Its design doc lands in
  `docs/design/` (see Documentation).

## Documentation

- Specs live in `docs/specs/`.
- Design docs live in `docs/design/`, numbered: `NNNN-short-slug.md` (e.g.,
  `0001-event-schema-and-command-surface.md`). A design doc describes how a
  piece of the system works; it may evolve as the system does.
- Decision records live in `docs/decisions/`, same numbering convention. A
  decision record captures a point-in-time choice and its rationale; records
  are append-only — supersede, don't rewrite.

## Architecture

- **lib/bin split**: `src/lib.rs` is the library; `src/main.rs` is a thin entry
  point (parse CLI, init telemetry, dispatch, shutdown). Put logic in the
  library, not in main.
- **Event-sourcing seams** (define early, even if stubbed):
  - `EventEnvelope` metadata: event id, stream/aggregate id, sequence/version,
    recorded/occurred timestamps, causation/correlation ids, schema version.
  - `EventStore` trait: `append(stream, expected_version, events)` +
    `load(stream)` / `replay()`.
  - Aggregate pattern: pure `decide(command, state) -> events` and
    `evolve(state, event) -> state`.
  - Projection/read-model layer for status/list views.
  - Schema versioning/upcasting plan before the first real event is written.
- These seams are directional guidance predating the increment plan. The design
  doc produced by Increment 0 (`docs/design/`) is authoritative once merged —
  update this section to match it at that point.
- Current modules are flat (`cli`, `commands`, `config`, `model`, `telemetry`);
  they will evolve toward `domain/`, `event_store/`, `projections/` as the
  event model lands.
- Config/data paths come from `directories::ProjectDirs`.

## Code Conventions

### Errors

Use **miette** everywhere. `Result<T> = miette::Result<T>`,
`miette::bail!()` / `miette::miette!()` for errors, `.into_diagnostic()?` to
convert std/other errors. `miette::set_panic_hook()` in main. Unimplemented
commands must fail loudly with `miette::bail!`, not `todo!()` or silent success.

### Logging & telemetry

- Use `tracing` for logging: `#[instrument]` on functions (with `skip()` for
  sensitive args, `fields()` for structured fields), `info!` / `debug!` macros.
- Logs go to **stderr**; stdout is reserved for command output.
- Telemetry is **opt-in** and behind the `telemetry` feature. Never fail a
  command because telemetry init/shutdown/export failed. Honor
  `OTEL_SDK_DISABLED`. Keep the exporter timeout short. Redact header values
  before logging them.

### Imports

Imports must be at the **top level** (top of the file), except:

- (a) in tests — imports may be at the top of the `mod tests` block;
- (b) when there is a real, technical reason to scope the import (e.g.,
  `#[cfg(...)]`-gated imports, or a name collision).

### Tests

- Every increment PR ships tests for its stage: unit tests for gate/scoring
  logic, round-trip/golden tests for the JSONL event log.
- `#[cfg(test)] mod tests { use super::*; ... }` at the bottom of each module.
- Descriptive snake_case test names.
- Group tests with section comments (`// ── Name ────`).

### Formatting & lints (before every commit)

- `cargo fmt` (rustfmt) for Rust.
- `prettier --write` for Markdown/JSON.
- `cargo clippy -- -D warnings` must pass.

## Commands

| Command                       | Purpose           |
| ----------------------------- | ----------------- |
| `cargo check`                 | Type-check        |
| `cargo test`                  | Run tests         |
| `cargo clippy -- -D warnings` | Lint (must pass)  |
| `cargo fmt --check`           | Verify formatting |
| `cargo build`                 | Build             |

## Pebble

This repo uses Pebble for issue tracking (prefix `GWLJ`). Follow the worktree
discipline from the global agent instructions: feature work in linked
worktrees, `.pebble` changes committed only from the primary checkout.

Pebbles map 1:1 to delivery increments. Claim the pebble before starting work,
reference its ID (e.g., `GWLJ-xxxxxx`) in the PR description, and close it
with `--reason` when the PR merges.
