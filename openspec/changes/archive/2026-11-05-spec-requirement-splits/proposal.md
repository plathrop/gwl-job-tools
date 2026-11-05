# Proposal

## Why

openspec 1.14+ enforces a strict rule that a requirement's text must not
exceed 500 characters, and four requirements in the main specs predate it:
three in `discovery` (571, 515, and 1208 characters) and one in
`event-replay` (522). With openspec now pinned in `mise.toml`, those
warnings are reproducible for every contributor, so `openspec validate
--all --strict` fails on a clean `main`. A validation step nobody can
pass is a validation step everybody ignores.

The over-long requirements were never wrong, just overloaded: each one
states several independent behaviors and buries edge cases in its prose.
Splitting them restores the property the strict rule is protecting — one
behavior per requirement, with edge cases as scenarios.

## What Changes

- Split `discovery` / "Discovery reports credit exhaustion" into two
  requirements: truncation is reported, and a payment-required shortfall
  is a lower bound.
- Split `discovery` / "A partial paid fetch keeps its already-fetched
  records" into two requirements: the salvage itself, and the rule that a
  payment-required response is a partial fetch at any page.
- Split `discovery` / "Paid fetches are bounded by the credit balance"
  into five requirements: per-page capping, offset continuation, a zero
  balance before any record, exhaustion mid-fetch settled at the source,
  and a balance-check failure not blocking the fetch.
- Split `event-replay` / "Replay fails loudly on malformed payloads" into
  two requirements: the consumed payload families fail replay, and known
  event types the aggregate cannot consume are validated by the
  read-model projection.
- Keep every existing scenario verbatim (openspec refuses a `MODIFIED`
  block that drops one, and each scenario stays with the requirement whose
  behavior it demonstrates); give each extracted behavior its own scenario.

No requirement's normative meaning changes, and no scenario's expected
behavior changes. This is prose-level spec hygiene, not a behavior
change, and deliberately carries no code.

## Capabilities

### New Capabilities

None.

### Modified Capabilities

- `discovery`: requirement texts split; requirement set grows from 20 to
  26. Every original scenario is retained; the extracted requirements add
  one scenario each (37 to 43).
- `event-replay`: requirement text split; requirement set grows from 2 to
  3. Every original scenario is retained; the extracted requirement adds
  one (5 to 6).

## Impact

- Specs only: `openspec/specs/discovery/spec.md` and
  `openspec/specs/event-replay/spec.md` (via this change's delta specs).
- No code, config, API, or dependency changes. No runtime behavior
  changes.
- Makes `openspec validate --all --strict` exit 0, which is the
  precondition for requiring it in CI.
