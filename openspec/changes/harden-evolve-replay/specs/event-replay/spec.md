## Purpose

Defines the contract for replaying the append-only event log into in-memory
state: corruption is a hard error, never a silent skip or partial state.

## ADDED Requirements

### Requirement: Replay fails loudly on malformed payloads

When replaying an event whose type is known, a payload that does not
deserialize into that type's expected shape is source-of-truth corruption.
The system MUST fail replay with a hard error identifying the event, and
MUST NOT leave partially-applied state (e.g. a lead marked "existing" with
no snapshot, adapter, URL, or text).

#### Scenario: Legacy snapshot payload fails replay

- **WHEN** an `ingested`/`updated`/`edited` event's payload lacks the
  required `adapter` field (e.g. a pre-rename payload carrying only
  `source`)
- **THEN** replay fails with a hard error naming the event, and the lead is
  not left in a partial "exists with no snapshot" state

#### Scenario: Malformed scored payload fails replay

- **WHEN** a `scored` event's payload cannot deserialize into the expected
  score shape
- **THEN** replay fails with a hard error naming the event

#### Scenario: Malformed reviewed payload fails replay

- **WHEN** a `reviewed` event's payload lacks a string `mark`
- **THEN** replay fails with a hard error naming the event

### Requirement: Unknown event types are ignored

An event whose type this build does not recognize is forward-compatible
data, not corruption. Replay MUST ignore it (advancing the sequence without
applying state) so a newer build's events do not break an older build.

#### Scenario: Unknown event type is skipped

- **WHEN** replay encounters an event whose `type` is not recognized
- **THEN** the event is ignored and replay continues without error
