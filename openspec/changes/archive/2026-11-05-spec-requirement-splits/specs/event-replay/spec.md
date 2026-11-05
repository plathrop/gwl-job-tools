# Spec Delta

## MODIFIED Requirements

### Requirement: Replay fails loudly on malformed payloads

When the aggregate replay consumes a snapshot (`ingested`/`updated`/
`edited`), `scored`, or `reviewed` event — the payload families it decodes —
a payload that does not deserialize into its expected shape is
source-of-truth corruption. The system MUST fail replay with a hard error
identifying the event, and MUST NOT leave partially-applied state (e.g. a
lead marked "existing" with no snapshot, adapter, URL, or text).

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

#### Scenario: Malformed suppressed payload fails replay

- **WHEN** a `reingest_suppressed` event's payload lacks the required
  `dedupe_key` (a type the aggregate does not consume)
- **THEN** replay fails with a hard error naming the event, via the
  read-model projection's validation

## ADDED Requirements

### Requirement: Unconsumed known event types are validated by the projection

A known event type the aggregate does not consume SHALL be validated by the
read-model projection, and SHALL NOT be dismissed as forward-compatible
unknown data, so a malformed payload in that family still fails replay
loudly.

#### Scenario: A known-but-unconsumed event is not treated as unknown

- **WHEN** replay encounters an event type the aggregate does not consume but
  the projection recognizes, and its payload is malformed
- **THEN** the event is not skipped as unrecognized data, and replay fails
  with a hard error naming the event
