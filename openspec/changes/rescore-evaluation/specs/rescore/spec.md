# Spec Delta

## Purpose

Re-running the gate + score evaluation over already-ingested leads
against the current configuration, so that evaluation results baked into
the event log can be corrected in place — one lead at a time or as a
batch over the visible pipeline.

## ADDED Requirements

### Requirement: Rescore re-evaluates a single lead with the current config

The `rescore` command SHALL re-run gates and scoring for one lead,
using the current configuration against the lead's latest stored
snapshot (including the stored posting text, without refetching), and
append one evaluation to the event log: one `rejected` event per
failed gate, otherwise a single `scored` event. No snapshot event is
appended, and the lead's marks and outcomes are untouched.

#### Scenario: A lead re-scores to a new composite

- **WHEN** the operator runs `rescore <lead>` on a scored lead and the
  current config yields a different composite than the stored score
- **THEN** a new `scored` event is appended with the next evaluation
  revision, and the lead's projected score is the new composite

#### Scenario: A previously passing lead newly rejects

- **WHEN** the operator runs `rescore <lead>` on a scored lead and a
  gate now fails under the current config
- **THEN** a `rejected` event is appended with the next evaluation
  revision, the lead leaves the default `list` view as gate-rejected,
  and the lead remains `rescore`-able and `edit`-revivable

#### Scenario: A gate-rejected lead revives

- **WHEN** the operator runs `rescore <lead>` on a lead whose latest
  evaluation failed a gate, and the current config passes every gate
- **THEN** a `scored` event is appended with the next evaluation
  revision, and the lead re-enters the default `list` view

#### Scenario: A no-change re-evaluation still appends

- **WHEN** the operator runs `rescore <lead>` and the re-evaluation
  produces the same result as the stored evaluation
- **THEN** the evaluation event is still appended (a re-evaluation
  happened; the log records it) and the command reports a zero-delta

#### Scenario: Rescore requires a target

- **WHEN** the operator runs `rescore` with neither a lead prefix nor
  `--all`
- **THEN** the command fails loudly with a usage error

### Requirement: Rescore --all covers the visible pipeline

`rescore --all` SHALL re-evaluate every lead that is neither terminal
nor durably ignored. Gate-rejected leads SHALL be included; ignored and
terminal leads SHALL be excluded.

#### Scenario: Batch scope

- **WHEN** the operator runs `rescore --all` on a corpus containing
  pending, deferred, applied, gate-rejected, ignored, and terminal leads
- **THEN** every non-terminal, non-ignored lead is re-evaluated
  (gate-rejected included), and no ignored or terminal lead is touched

#### Scenario: Batch summary

- **WHEN** a `rescore --all` run completes
- **THEN** the command reports the number of leads evaluated, the
  number whose composite score changed, the number newly rejected, the
  number revived, and the number unchanged — and `--json` output
  carries the same counts

### Requirement: Rescore --dry-run appends nothing

`rescore --dry-run` SHALL evaluate the same lead set with the current
config but append no events and take no writer lock, and SHALL report
the same summary plus which leads would change, newly reject, or
revive.

#### Scenario: Dry-run preview

- **WHEN** the operator runs `rescore --all --dry-run`
- **THEN** the would-change summary is printed per lead and in total,
  the event log is byte-identical before and after, and no discovery or
  writer state is created

### Requirement: Rescore emits observability signals

A rescore invocation SHALL emit a span covering the run with the
evaluated/changed/rejected/revived counts as fields, per-lead debug
records carrying the old and new composite, and an info summary line
matching the command output. Telemetry remains opt-in per the
existing telemetry policy.

#### Scenario: Telemetry off

- **WHEN** telemetry is not enabled
- **THEN** the rescore span and logs go to the log file sink only, and
  the command succeeds identically
