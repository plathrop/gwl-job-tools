# AGENTS.md

Guidance for AI agents (and humans) working on `gwl-job-tools`.

## Project Overview

`gwl-job-tools` is a CLI job-search triage and application-assist tool. It
automates the high-volume, low-judgment work of a job search (sift, score,
capture metadata, prepare application materials) and reserves human judgment
for the high-value decisions (which jobs to pursue).

The living specification lives in `openspec/specs/` (reachable via the
`docs/specs/` symlink); the original v2 prompt is archived at
**`docs/archive/job-search-automation-v2.md`**. Read the archived prompt
before starting work. This file carries the always-relevant context; the
specs carry the scope.

## Non-Negotiable Guardrails

These come from the spec and apply to every change:

- Never auto-submit an application without explicit per-job approval. The
  "apply-automatically" mark *is* that approval; the final browser click is
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

- Deliver in small increments, each as a PR (an increment = an OpenSpec change;
  see OpenSpec). PRs are the review checkpoint.
- When opening a PR, add @remi-ashe as a reviewer.
- The agent may continue ahead via stacked PRs, capped at 2.
- Before opening a new stacked PR, check for review feedback (Copilot, Remi,
  human) on existing PRs and incorporate it.
- Review cast: Copilot auto-reviews with inline comments; Remi is a required
  reviewer; Grey may also post folded third-party reviews (e.g., deepseek) as
  PR comments from their own account — treat those as review feedback, not as
  Grey's own inline comments.
- **Merging**: merge locally with fast-forward — never through GitHub. The
  flow and rationale live in the global agent instructions ("Git merges:
  local fast-forward only"). This repo enforces it by construction: merge
  commits are the only GitHub merge method enabled, and the `main` ruleset
  requires linear history (which merge commits violate) plus signed
  commits — so every GitHub-side merge fails and unsigned pushes are
  rejected. Pushing `main` after a local ff-merge marks the PR merged and
  auto-deletes the branch; local cleanup is `git worktree remove`,
  `git branch -d`, and `git fetch --prune`.

## Observability

Instrumentation is part of this project's contract, not an afterthought. The
policy and its rationale live in
`docs/decisions/0011-observability-by-default-for-agent-changes.md`; concrete
conventions live under "Logging & telemetry" in Code Conventions. This section
is the operating procedure.

**Load skills conditionally.** If your change adds or modifies application
logic (commands, domain, event store, projections, ingest), load the
`otel-instrumentation` skill before writing code. Load
`observability-fundamentals` only when the change involves a design decision
about *what* to instrument (new subsystem, new failure modes). Docs-only,
spec-only, and config-only changes need neither. Scope instrumentation to the
code you're touching — never retrofit unrelated code.

**Instrumentation floor** (minimum for code you touch):

- `#[instrument]` on public functions doing I/O or crossing module boundaries,
  with `skip()` for sensitive args.
- A span around every external call (event-log file I/O, future network
  fetches) with outcome fields.
- `info!` on command start/finish and significant decisions; `debug!` for
  detail; `warn!`/`error!` always with identifying fields (job id, path, event
  type) so the offending input can be located.

**OpenSpec changes must spec observability.** Proposals for behavior-bearing
changes must state what an operator needs to see to answer "did this work, and
how well?" — which spans/logs/metrics the feature should emit — or explicitly
justify why none is needed. Apply the same test in `design.md`.

**PR gate.** Every PR description must include an **Observability** section:
what instrumentation the change adds, or one line on why none is needed. A
missing section is an incomplete PR; reviewers should treat it as such.

## Documentation

- **Specs** — the living contract of what the system must do — live in
  `openspec/specs/`, modified only through OpenSpec changes. `docs/specs/` is
  a symlink to that directory for convenience.
- **Design docs** live in `docs/design/`, numbered: `NNNN-short-slug.md` (e.g.,
  `0001-event-schema-and-command-surface.md`). A design doc describes how a
  piece of the system works; it may evolve as the system does.
- **Decision records** live in `docs/decisions/`, same numbering convention. A
  decision record captures a point-in-time choice and its rationale; records
  are append-only — supersede, don't rewrite.
- **Historical/superseded documents** (e.g., the original v2 prompt) live in
  `docs/archive/` — kept for provenance, not authority.

## Architecture

- **lib/bin split**: `src/lib.rs` is the library; `src/main.rs` is a thin entry
  point (parse CLI, init telemetry, dispatch, shutdown). Put logic in the
  library, not in main.
- **Event-sourcing seams**: `docs/design/0001-event-schema-and-command-surface.md`
  is authoritative. It defines the `EventEnvelope` metadata, the `EventStore`
  trait, the aggregate `decide`/`evolve` pattern, projections, and the schema
  versioning/upcasting plan.
- Modules evolve from flat (`cli`, `commands`, `config`, `model`, `telemetry`)
  toward `domain/`, `event_store/`, `projections/`, `ingest/` per the layout
  in design doc 0001 §9.
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
- Logs go to a **file sink** (decision 0005), not stdout/stderr; stdout is
  reserved for command output and stderr for miette error reports.
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
- `dprint fmt` for Markdown, TOML, YAML, and JSON.
- `cargo clippy -- -D warnings` must pass.

## Commands

| Command                       | Purpose           |
| ----------------------------- | ----------------- |
| `cargo check`                 | Type-check        |
| `cargo test`                  | Run tests         |
| `cargo clippy -- -D warnings` | Lint (must pass)  |
| `cargo fmt --check`           | Verify formatting |
| `cargo build`                 | Build             |

## OpenSpec

Change management uses the spec-driven workflow, under `openspec/`:

- **Specs** (`openspec/specs/`) are the living contract of what the system
  does. Modify them only through changes, never by hand.
- **Changes** (`openspec/changes/`) carry `proposal.md`, `specs/`, `design.md`,
  and `tasks.md`. Lifecycle: propose → apply → archive.
- Drive it with the openspec skills (`.pi/skills/openspec-*`).

One change = one delivery increment = one PR. Break large specs into multiple
small changes rather than one large one. A change's `design.md` is transient
and archived with the change; durable architecture lives in `docs/design/`
(see Documentation).

## Pebble

This repo uses Pebble (prefix `GWLJ`) as the **backlog and in-flight ledger** —
the complement to OpenSpec, not a second planning layer. Follow the worktree
discipline from the global agent instructions: feature work in linked
worktrees, `.pebble` changes committed only from the primary checkout.

Pebbles track:

- ideas not yet ready to become specs;
- bugfixes (no spec needed);
- epics — an arc of work or a bucket (e.g., the "Bug Fixes" epic);
- breaking a large spec into small deliverable chunks;
- tasks that surface during execution of a change but don't belong in it.

Claim the pebble before starting work, reference its ID (e.g., `GWLJ-xxxxxx`)
in the PR description, and close it with `--reason` when the PR merges. A
change may close several pebbles or none; pebbles don't map 1:1 to changes.
