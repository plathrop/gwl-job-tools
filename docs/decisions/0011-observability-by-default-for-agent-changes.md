# 0011: Observability by default for agent-driven changes

Status: accepted 2026-09-16 (rewriting the AGENTS.md observability guidance)

## Context

Agents making changes to this repo do not consistently include
observability work — spans, logs, and telemetry requirements tend to be
skipped unless explicitly demanded, because the cost of skipping is zero.

A first attempt to fix this (a pending AGENTS.md edit) was exhortative:
"observability is a key part of this project," "well instrumented," "good
log coverage," plus an unconditional instruction to load the
`observability-fundamentals` and `otel-instrumentation` skills before
*any* change. Exhortation gives an agent no actionable criteria and no
way to know when it has complied; unconditional skill loading fires on
docs typo fixes and therefore gets ignored. Neither changes behavior.

Existing machinery this decision builds on:

- `tracing`-based logging to a file sink, configurable level
  (decision 0005); OTLP export to Honeycomb behind the `telemetry`
  feature (opt-in, `OTEL_SDK_DISABLED` honored).
- Global agent instructions already require instrumenting boundaries of
  code being changed (not retrofits of untouched code) and provide the
  `otel-instrumentation` and `observability-fundamentals` skills.
- OpenSpec carries the delivery workflow: proposals, designs, and
  PRs are the artifacts an agent already produces and that reviewers
  already inspect.

## Decision

1. **Instrumentation is part of the contract** for behavior-bearing
   changes, scoped to the code being changed. No retrofits of unrelated
   code (per the global agent instructions).
2. **Skill loading is conditional.** Load `otel-instrumentation` when
   adding or modifying application logic (commands, domain, event store,
   projections, ingest). Load `observability-fundamentals` only when the
   change involves a design decision about *what* to instrument (new
   subsystem, new failure modes). Docs-only, spec-only, and config-only
   changes load neither.
3. **An instrumentation floor applies to code touched:**
   - `#[instrument]` on public functions doing I/O or crossing module
     boundaries, with `skip()` for sensitive args.
   - A span around every external call (event-log file I/O, future
     network fetches) with outcome fields.
   - `info!` on command start/finish and significant decisions; `debug!`
     for detail; `warn!`/`error!` always with identifying fields (job id,
     path, event type) so the offending input can be located.
     The floor is a minimum, not a ceiling; concrete conventions live in
     AGENTS.md ("Logging & telemetry" under Code Conventions) and in the
     `otel-instrumentation` skill.
4. **OpenSpec changes spec observability up front.** Proposals for
   behavior-bearing changes state what an operator needs to see to
   answer "did this work, and how well?" — which spans/logs/metrics the
   feature should emit — or explicitly justify why none is needed. The
   same test applies in `design.md`.
5. **Enforcement rides the PR description.** Every PR must include an
   **Observability** section: what instrumentation the change adds, or
   one line on why none is needed. A missing section is an incomplete
   PR, and reviewers (Remi, Copilot) should treat it as such.

## Rationale

- **Gates beat exhortation.** Instructions change agent behavior when
  they attach to an artifact the agent already produces and that a
  reviewer already inspects. The PR-description section is that gate:
  the agent must either do the work or write down why it didn't.
- **Negative space forces evaluation.** Requiring "why none is needed"
  converts silent skipping into a decision the agent (and reviewer)
  can see and challenge.
- **Conditional triggers survive.** An instruction that fires on every
  change regardless of kind trains the agent to ignore it. Skill loading
  tied to "touching application logic" or "making an instrumentation
  design decision" fires rarely enough to be respected.
- **Shift-left is cheaper than retrofit.** Asking "what should an
  operator see?" during proposal — when the failure modes are being
  designed — is cheaper and more accurate than asking after
  implementation.

## Consequences

- AGENTS.md carries an operational summary of this policy (when to load
  skills, the floor, the openspec requirement, the PR gate); rationale
  stays here.
- Remi and Copilot reviews gain a standing check: PR descriptions
  without an Observability section should be flagged.
- The floor list may grow (e.g., metric counters once metrics are
  introduced); update AGENTS.md, not this record. Supersede this record
  if the policy itself changes (e.g., dropping the PR-description gate
  in favor of CI enforcement).
- If the gate proves noisy (boilerplate sections on trivial PRs), the
  escape valve is the "one line on why none is needed" clause — the
  section stays, its content shrinks.
