# Design

## Context

See `proposal.md` — Why. The change is prose-only: it reshapes four
requirement blocks in two main specs so each states one behavior and fits
openspec's 500-character strict rule.

The controlling constraint is openspec's delta model. A `MODIFIED`
requirement replaces the existing block wholesale at archive time, and
`openspec validate` refuses a `MODIFIED` block that drops any scenario the
current spec still has. A split therefore cannot be expressed as "move
requirement text and its scenarios into several new requirements": the
original requirement would lose scenarios, and validation rejects it.

## Goals / Non-Goals

**Goals**

- `openspec validate --all --strict` exits 0 on `main`.
- One behavior per requirement, in the four over-long blocks.
- No scenario text, requirement meaning, or expected behavior changes.

**Non-Goals**

- Reorganizing the specs beyond the four flagged requirements.
- Changing any code, config, or runtime behavior.
- Adding `openspec validate` to CI (a follow-up; this change is its
  precondition).

## Decisions

### Split by `MODIFIED` + `ADDED`, not `REMOVED` + `ADDED`

The validator's scenario-retention rule makes `MODIFIED` + `ADDED` the only
mechanism that works: the original requirement keeps its header and every
scenario, its description is cut to the single behavior it keeps, and each
extracted behavior becomes its own `ADDED` requirement with its own
scenario. `REMOVED` + `ADDED` would allow a cleaner scenario-to-behavior
mapping, but it discards the original requirement names, which nothing
here is deprecating, and it requires inventing a `Migration` note for what
is really a prose split. Chosen: `MODIFIED` + `ADDED`.

**Consequence, accepted**: a few scenarios now describe the same behavior
twice — once under the retained requirement (where the original scenario
must stay) and once under the extracted requirement (which needs a
scenario of its own). The duplicates are worded from different angles and
neither contradicts the other. Removing the duplication would require
first splitting the original into named requirements and then removing
the old one in a later change, which is not worth a second change for
prose hygiene.

### Which requirement keeps which behavior

The retained requirement is the one whose name still describes the
behavior it keeps:

| Capability   | Requirement                                            | Keeps                                                                  | Extracts                                                                                 |
| ------------ | ------------------------------------------------------ | ---------------------------------------------------------------------- | ---------------------------------------------------------------------------------------- |
| discovery    | Discovery reports credit exhaustion                    | reporting truncation, from a marker or a 402                           | the 402 shortfall is a lower bound                                                       |
| discovery    | A partial paid fetch keeps its already-fetched records | salvage after records were fetched; pre-record failure is whole-source | a 402 is a partial fetch at any page                                                     |
| discovery    | Paid fetches are bounded by the credit balance         | per-page capping                                                       | offset continuation, mid-fetch settlement, pre-fetch zero balance, balance-check failure |
| event-replay | Replay fails loudly on malformed payloads              | the consumed payload families                                          | unconsumed known types are the projection's job                                          |

### Scenarios stay attached, not rewritten

Every existing scenario is copied verbatim into the retained block. New
`ADDED` requirements state the extracted behavior and get one scenario
each, written to test that behavior specifically rather than to restate
the retained block's scenario.

## Risks / Trade-offs

- **[Scenario duplication]** → Accepted above; wording differs and no
  assertion conflicts. A future cleanup can drop the redundant copies once
  the retained requirements no longer need them.
- **[Requirement order shifts]** → Archive appends `ADDED` requirements, so
  the paid-feed requirements no longer sit contiguously. Order carries no
  meaning in a spec; accept it rather than hand-editing after archive.
- **[Silent meaning drift while cutting descriptions]** → Mitigated by
  checking each extracted behavior lands in exactly one `ADDED`
  requirement and by diffing the archived spec against the pre-change text
  sentence by sentence.

## Migration Plan

None. No behavior, API, or data changes. Rollback is a `git revert`.

## Open Questions

None.
