# Tasks

## 1. Split the `discovery` requirements

- [x] 1.1 Cut "Discovery reports credit exhaustion" to reporting-only text and
      keep its three scenarios verbatim; confirm the block's description is under
      500 characters with `openspec validate spec-requirement-splits --strict`.
- [x] 1.2 Cut "A partial paid fetch keeps its already-fetched records" to the
      salvage behavior and keep both scenarios verbatim; add the `ADDED`
      payment-required-at-any-page requirement with its own scenario; confirm
      strict validation passes and both extracted behaviors are stated once.
- [x] 1.3 Cut "Paid fetches are bounded by the credit balance" to per-page
      capping and keep all five scenarios verbatim; add the four `ADDED`
      requirements (offset continuation, mid-fetch settlement, pre-fetch zero
      balance, balance-check failure), each with its own scenario; confirm every
      sentence of the original text has exactly one home.

## 2. Split the `event-replay` requirement

- [x] 2.1 Cut "Replay fails loudly on malformed payloads" to the consumed
      payload families and keep all four scenarios verbatim; add the `ADDED`
      read-model-projection requirement with its own scenario; confirm the block
      is under 500 characters.

## 3. Verify the whole spec set

- [x] 3.1 `mise exec -- openspec validate --all --strict` exits 0 with zero
      warnings on the working tree.
- [x] 3.2 Diff the archived `discovery` and `event-replay` specs against the
      pre-change text sentence by sentence: no requirement meaning changed, no
      scenario dropped or reworded.

## Workflow follow-up

- Archive the change so the deltas land in `openspec/specs/`; re-run
  `openspec validate --all --strict`.
- Reference `GWLJ-jnqpjs` and `GWLJ-h9392m` in the PR; close both on merge.
