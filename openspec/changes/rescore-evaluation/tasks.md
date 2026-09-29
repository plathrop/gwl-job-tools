# Tasks

## 1. Decide path: re-evaluate without a snapshot event

- [ ] 1.1 Add `decide_reevaluate(state, gate_failures, score)` to `src/domain/lead.rs`: validate the XOR invariant (rejection XOR score, mirroring `decide_edit`), then append `rejection_events`/`scored_event` with `revision + 1` and no snapshot event. Verify: unit tests cover the pass (scored appended, revision bumped), fail (rejected appended), and the both/neither bail cases.
- [ ] 1.2 Unit-test the projection over a re-evaluation sequence: ingested → scored → rescored (new composite) → projected composite is the new one; ingested → rejected → rescored (passing) → lead leaves the gate-rejected state and re-enters `active_leads`. Verify: projection tests green; the revival case exercises decision 0013's `is_gate_rejected` returning false.

## 2. Command layer: `rescore` single-lead and batch

- [ ] 2.1 Create `src/commands/rescore.rs`: one lead — resolve prefix via the projection, run `gates::evaluate` + `scoring::score` with the current config over the stored snapshot (`raw_text` or empty string, as `edit` does), `decide_reevaluate`, append under the single-writer store, print the per-lead result card/summary. Verify: unit test asserts the appended event sequence and printed delta for change/new-reject/revive/no-change cases.
- [ ] 2.2 Add `--all`: iterate `active_leads()` (non-terminal, non-ignored, gate-rejected included per decision 0013 scope), evaluate each, append per lead, and report the batch summary (evaluated / changed / newly rejected / revived / unchanged). `--json` output carries the same counts. Verify: unit test over a mixed corpus (pending, gate-rejected, ignored, terminal) asserts only in-scope leads are touched and the counts are right.
- [ ] 2.3 Add `--dry-run`: build the projection via `read_only_envelopes` (no writer lock), evaluate each target in memory via the same shared evaluation helper, print the would-change summary, append nothing. Verify: test asserts the event log is byte-identical after a dry run and the summary matches a real run's on a fixed corpus.
- [ ] 2.4 Wire the command into `src/cli.rs` (Triage group: `rescore [lead?] [--all] [--dry-run]`, requiring a target or `--all`) and dispatch; `cmd_label` = "rescore". Verify: parse tests for `rescore abc`, `rescore --all`, `rescore --all --dry-run`, and the bare `rescore` usage error.

## 3. Observability

- [ ] 3.1 Add the `rescore` span with `evaluated`/`changed`/`rejected`/`revived` fields (and `dry_run`), per-lead `debug` records with lead id + old/new composite, and an `info` summary matching the command output. Verify: a test or `--log-level debug` run shows the fields; `OTEL_SDK_DISABLED` behavior unchanged (telemetry stays opt-in).

## 4. Docs and finishing

- [ ] 4.1 README command table gains `rescore`; design doc 0001 §3 gains the re-evaluation rule (edit and rescore are the two re-evaluation paths over a stored snapshot; only edit appends a snapshot event). Verify: `dprint fmt` clean; docs render.
- [ ] 4.2 Run `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, `cargo test`, `dprint fmt`, and `openspec validate --strict`. Verify: all exit 0 and validate reports the change valid.
