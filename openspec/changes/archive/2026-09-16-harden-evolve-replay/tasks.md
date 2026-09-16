## 1. Make `evolve` strict

- [x] 1.1 Change `lead::evolve` to return `miette::Result<()>` and make the
      snapshot arm (`ingested`/`updated`/`edited`) decode `SnapshotFields` first,
      `?`-propagating on failure before mutating state — verify `cargo check`
      compiles and the existing snapshot evolve tests still pass.

- [x] 1.2 Make the `scored` arm decode `ScoredPayload` strictly, `?` on
      failure — verify the existing scored evolve test passes.

- [x] 1.3 Make the `reviewed` arm decode `ReviewedPayload` strictly, replacing
      the lenient `.get("mark").and_then(Value::as_str)` extraction — verify the
      existing reviewed evolve test passes.

## 2. Propagate the error to callers

- [x] 2.1 Update `commands::replay_lead` to `?`-propagate `evolve`'s result —
      verify `cargo check` passes and the existing `record_ingest` tests remain
      green.

## 3. Tests

- [x] 3.1 Update existing `lead.rs` test call sites to the new `Result`
      signature — verify `cargo test` is green.

- [x] 3.2 Add a test that a legacy `source`-without-`adapter` snapshot payload
      makes `evolve` error and leaves no partial state — verify the test passes.

- [x] 3.3 Add a test that a malformed `scored` payload makes `evolve` error —
      verify the test passes.

- [x] 3.4 Add a test that a `reviewed` payload missing a string `mark` makes
      `evolve` error — verify the test passes.

- [x] 3.5 Confirm the existing unknown-event-type forward-compat test still
      passes (an unknown `type` is ignored, no error) — verify it passes.

## 4. Verification

- [x] 4.1 Run `cargo fmt --check`, `cargo clippy -- -D warnings`, and
      `cargo test` (including `--no-default-features`) — verify all green.
