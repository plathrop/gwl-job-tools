## 1. Redirect-aware canonicalization (ingest layer)

- [ ] 1.1 Capture the post-redirect final URL in `HttpFetcher::get` (read `Response::url()`) and surface it on the fetch result; update the `ScriptedFetcher` test mock and its `response()` helper for the new field. Verify: a unit test with a scripted redirect asserts the final URL is returned, and existing `ingest` tests still pass.
- [ ] 1.2 Re-run `platforms::detect` on the resolved final URL in `ingest_url` and record the canonical (resolved) URL in `IngestOutcome.url`. Verify: a test that a proxy URL redirecting to `boards.greenhouse.io/...` ingests via the Greenhouse API (`adapter == "greenhouse"`) with the canonical URL recorded, not the proxy.

## 2. Discovery seam

- [ ] 2.1 Add the `Posting` struct and `DiscoverySource` trait (native `async fn`, edition 2024) in a new `discovery` module. Verify: crate compiles with the trait in place.
- [ ] 2.2 Add the `[sources.<name>]` config section with an `enabled` flag (default disabled) and load/validate it. Verify: config tests cover a disabled-by-default source, an enabled source, and rejection of unknown keys.

## 3. Remotive adapter

- [ ] 3.1 Implement the Remotive `DiscoverySource` parsing its public JSON feed into `Posting`s. Verify: a unit test over a saved feed fixture asserts the parsed postings (URL, title, company, comp, location).

## 4. Batch driver + `discover` command

- [ ] 4.1 Add the `discover` subcommand to the CLI (runs all enabled sources; `--source <name>` runs one). Verify: `gwl-jobs discover --help` renders, and no-enabled-sources is a clean no-op with a message.
- [ ] 4.2 Implement the batch driver: fetch + extract postings without the writer lock, then acquire the lock once and loop `record_ingest`, re-projecting between postings. Verify: an integration test where two postings denote the same job produces exactly one lead stream (second is a re-ingest, not a duplicate).
- [ ] 4.3 Make per-posting fetch/extract failures non-fatal: log with the URL, count as `failed`, continue the batch. Verify: a test with one dead posting among healthy ones continues and reports the failure.
- [ ] 4.4 Emit the batch summary (new / updated / suppressed / rejected / failed) in human and `--json` output. Verify: a test asserts the summary counts match the events appended.
- [ ] 4.5 Confirm idempotent re-runs and durable-ignore suppression through the discovery path. Verify: an integration test re-runs the same feed (no new leads) and one where a posting matches an `ignore`d lead (suppressed, not re-queued).

## 5. Observability and finishing

- [ ] 5.1 Instrument per design.md: a span per source fetch and per posting ingest, `info!` run start/finish with summary counts, `warn!`/`error!` with the offending URL. Verify: manual run against a fixture feed shows the spans and summary in the log.
- [ ] 5.2 Run `cargo fmt`, `dprint fmt`, and `cargo clippy -- -D warnings` clean, and confirm the full `cargo test` suite passes. Verify: all three commands exit 0.
