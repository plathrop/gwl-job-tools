# Code Audit — gwl-job-tools, September 2026

Pebble: GWLJ-4png30 · Date: 2026-09-04 · Audited revision: `main` @ `8575bae`

Analysis only. This document is a set of recommendations; on approval the
repo executes it. Nothing here has been changed in code.

## Scope and method

Audited: all of `src/` (12,670 lines across 20 files — the 2,870-line
`commands/mod.rs`, the 1,683-line `projections/mod.rs`, the 1,305-line
`domain/lead.rs`, and the rest), `docs/` (the spec, both design docs, all
ten decision records, both READMEs), `Cargo.toml`, `README.md`, and
`AGENTS.md`. The full test suite (318 tests) passes on the audited
revision; `cargo clippy -- -D warnings` is clean. Every finding below was
verified against source at the cited line numbers on the audited
revision.

Severity:

- **High** — structural; affects maintainability at the current growth
  rate.
- **Medium** — inconsistency or drift that misleads a reader or reviewer.
- **Low** — polish; do opportunistically.

Effort: **S** ≤ half a day, **M** = half to two days, **L** > two days.

---

## High severity

### H1. `commands/mod.rs` is a monolith that design doc 0001 §9 says should be one module per command

**Evidence:**

- `src/commands/mod.rs` — 2,870 lines: 1,351 production, 1,519 test.
  97 `fn` definitions in the file, ~40 of them outside the test module.
- `docs/design/0001-event-schema-and-command-surface.md:580` (§9 module
  layout): `commands/ # one module per command; thin, calls domain`.
- The module's own doc comment (`src/commands/mod.rs:1`) says "Thin: I/O
  wiring around the domain" — at 14 commands, 4 output structs
  (`IngestSummary`, `EditSummary`, `QueueEntry`, `EditSpec`), the
  interactive review loop, browser launching, completion generation, and
  apply-package assembly, it is no longer thin.

**Why it matters:** every command change touches the same file, review
diffs span unrelated commands, and the file is the single largest merge
conflict surface in the repo. The design doc and the code disagree about
the layout, and the doc is supposed to be authoritative.

**Recommendation:** split into one file per command family, moving each
command's tests with it (the existing `// ── section ──` comments already
partition them):

```
commands/
  mod.rs         # shared: EVENT_LOG_NAME, replay_lead, select_lead,
                 #         open_url (+ cfg(test) stub + OPENED_URLS),
                 #         parse_occurred_at, record_outcome
  ingest.rs      # execute_ingest, record_ingest, IngestSummary
  show.rs        # execute_show, load_raw_text
  list.rs        # execute_list, QueueEntry, queue_entry
  mark.rs        # execute_mark, mark_lead
  edit.rs        # execute_edit, build_edit_spec, record_edit, EditSpec,
                 #            EditSummary, thousands
  package.rs     # execute_package, prepare_package, cheat_sheet
  review.rs      # execute_review, review_loop, read_review_key, RawModeGuard
  completion.rs  # execute_completion, write_completions, shell_from_name,
                 #                infer_shell
  outcome.rs     # execute_applied/screened/interviewed/offered/outcome,
                 #            resolve_apply_method
  events.rs      # execute_events, filter_events
```

Pure moves, no signature changes; `cli::execute` dispatch is unchanged.
**Effort: M.**

### H2. Store + projection boilerplate repeated in 13 commands

**Evidence:** the three-line preamble
`JsonlEventStore::open(paths.data_dir().join(EVENT_LOG_NAME))` →
`store.replay()` → `projections::rebuild(...)` appears at
`src/commands/mod.rs:80-82`, `242-244`, `320-322`, `355-357`, `435-437`,
`745-747`, `851-853`, `1187-1188`, `1223-1224`, `1244-1245`, `1265-1266`,
`1291-1292`, `1313-1314`. The `EVENT_LOG_NAME` constant and the
`data_dir().join(...)` construction are each duplicated knowledge.

**Recommendation:** add `AppPaths::event_log(&self) -> PathBuf` in
`config.rs` (single source for the log path), and a commands-layer helper
`fn open_workspace(paths: &AppPaths) -> Result<(JsonlEventStore, Projection)>`.
Fold into the H1 split. **Effort: S** (do together with H1).

### H3. The five outcome commands are copy-paste variants

**Evidence:** `execute_applied` (`src/commands/mod.rs:1186-1209`),
`execute_screened` (`1222-1240`), `execute_interviewed` (`1243-1261`),
`execute_offered` (`1264-1281`), `execute_outcome` (`1284-1309`) share
the identical open → rebuild → parse `--at` → `record_outcome` →
`println!` skeleton, differing only in event type and payload fields.

**Recommendation:** with H2's helper the preamble collapses to one line
per command; the wrappers keep their per-command argument unpacking
(that's their job). Optionally a shared `OutcomeCmd` struct of the common
fields (`lead`, `note`, `at`) would let the five `*Args` structs shrink,
but the clap structs are clear as they are — collapse the boilerplate,
keep the arg structs. **Effort: S.**

---

## Medium severity

### M1. Design doc 0001 header still says `Status: proposed`

**Evidence:** `docs/design/0001-event-schema-and-command-surface.md:3` —
`Status: proposed (Increment 0, GWLJ-rk2cnb)`. Design doc 0002 and every
decision record say `accepted`; 0001 is the accepted authoritative
design (AGENTS.md:86-89 calls it authoritative).

**Recommendation:** change to `Status: accepted (Increment 0, GWLJ-rk2cnb)`.
**Effort: S.**

### M2. Design doc 0001 §5 has a copy-paste artifact: duplicated step 2, truncated sentence

**Evidence:** `docs/design/0001...md:450-451` — the line "2. Steps
through pending leads highest-score-first. For each: renders the"
appears twice in a row; the first occurrence is truncated mid-thought
and the second is the real one.

**Recommendation:** delete line 450. **Effort: S.**

### M3. Design doc 0001 §8 command table drift vs the shipped CLI

**Evidence:**

- Line 546: `ingest --file` says "Same, from a local HTML/PDF/text drop"
  — PDF is **not** shipped (deferred to GWLJ-mxyp63).
  `src/cli.rs:20` says "Local file to ingest (HTML or plain text)";
  `src/ingest/mod.rs:326-331` routes `.html`/`.htm` through extraction
  and treats everything else as plain text.
- Line 545: the `ingest <url>` row doesn't mention `--source`, which
  ships (`src/cli.rs:25-26`) and is documented in §3.
- Lines 553-556: the `applied`/`screened`/`interviewed`/`offered` rows
  omit `--note`, which ships on all four (`src/cli.rs:244-246`,
  `259-261`, `274-276`, `287-288`). §3 says "Each accepts optional
  `note`", so this is table terseness rather than a contradiction.
- The `list` row (line 547) doesn't describe the actual row shape (rank,
  score, title @ company, deferral count, status tag, lead prefix —
  `src/render.rs:136-159`); the status tag and lead prefix come from
  design doc 0002 and decision 0008 and are the user's addressing
  handle, worth stating.

**Recommendation:** update the four rows to match the shipped surface.
**Effort: S.**

### M4. Personal names in code comments (tracked by GWLJ-bypxri)

**Evidence (all verified by grep):**

- `src/commands/mod.rs:503` — "(Remi, PR #15 review)"
- `src/commands/mod.rs:777` — "(PR #17 review: Remi + kimi, verified
  live with a stubbed xdg-open)"
- `src/commands/mod.rs:1376` — "Regression (Remi's raw-fallback bug)"
  (test comment)
- `src/commands/mod.rs:2080` — "Remi (PR #15): a field both set and
  cleared must not silently…" (test comment)
- `src/domain/gates.rs:61` — "the content is empty until the LLM scorer
  (Remi) lands"
- `src/domain/scoring.rs:406` — "k3 review round 2 (PR #7)" (test
  comment)
- `src/resume.rs:3` — "too strict to parse Grey's actual resume.json"
- `src/projections/mod.rs:1413` — "(PR #16: Grey's rejected-vs-…)"
  (test comment)
- `src/projections/mod.rs:1485` — "settled with Grey 2026-08-31" (test
  comment)
- Test fixtures use "Grey"/`grey@example.com`:
  `src/commands/mod.rs:2752-2753`, `2773-2774`, `2788`;
  `src/domain/lead.rs:1224`; `src/resume.rs:152`.

**Why it matters:** comments should outlive the current cast; a public
repo's comments shouldn't assume the reader knows who Remi, kimi, or
Grey are. The PR references are useful history; the names are not.

**Recommendation:** keep the PR references, drop the names:
"(PR #15 review)" / "the LLM scorer (vNext)" / "the maintainer's
resume.json" / "settled in review 2026-08-31". Fixture names are
arbitrary strings — either keep or switch to a neutral
"Avery Example". **Effort: S.**

### M5. Dead commented-out code in `main.rs`

**Evidence:** `src/main.rs:55-71` — a commented-out `datafile()`
function "Kept to show how to create the data file." It is also stale:
it builds a `gwl-jobs.jsonl` filename, while the actual log is
`events.jsonl` (`src/commands/mod.rs:42`), so as a reference example it
teaches the wrong name.

**Recommendation:** delete. Git history is the archive. **Effort: S.**

### M6. `Cargo.toml` description and `README.md` undersell the tool

**Evidence:** `Cargo.toml:4` — "A simple job search tracker." The spec's
scope is an event-sourced triage pipeline with gates, scoring, a review
queue, and apply packages. `README.md` is two lines ("# gwl-job-tools /
Job Application Tracker for the CLI") — no install, no commands, no
config.

**Recommendation:** description → e.g. "Event-sourced job-search triage
CLI: ingest, gate, score, review, and track applications." README: add a
short "what it does" paragraph (adapt AGENTS.md:7-10), install
(`cargo install` from repo), the command table (copy the corrected §8
from M3), config keys, and a pointer to
`docs/specs/job-search-automation-v2.md`. **Effort: S** (description) /
**M** (README).

### M7. `cli.rs` contains two identical command-name match functions

**Evidence:** `Cli::command_name` (`src/cli.rs:424-442`) and
`cmd_label` (`src/cli.rs:478-496`) are byte-identical 15-arm matches
over `Commands`.

**Recommendation:** delete `cmd_label`; use `command_name` in the
`#[instrument]` field (`src/cli.rs:445`). **Effort: S.**

### M8. `pending_queue` duplicates `ranked_leads`' comparator

**Evidence:** `src/projections/mod.rs:274-278` copies the sort
comparator from `ranked_leads` (`229-233`) — composite descending,
`first_seen` tie-break. The `ranked_leads` doc comment (`226`) says
"one sort, so the two cannot drift (PR #16 review)" — but
`pending_queue` holds its own copy and can drift.

**Recommendation:** extract a `fn by_rank(a: &LeadRecord, b: &LeadRecord)
-> Ordering` and use it in both. **Effort: S.**

### M9. `show` names its positional `id`; every other command names it `lead`

**Evidence:** `src/cli.rs:58` (`pub id`) vs `69`, `193`, `240`, `255`,
`270`, `285`, `297`, `335` (all `pub lead`). The difference is visible
in `--help` and in the arg structs' doc comments.

**Recommendation:** rename `ShowArgs::id` → `lead` (one callsite:
`src/commands/mod.rs:246`). **Effort: S.**

### M10. `mark` ignores the global `--json` flag

**Evidence:** `execute_mark` (`src/commands/mod.rs:354-377`) always
prints pretty JSON; it never receives the flag (`src/cli.rs:465`).
Every other human-facing command (ingest, show, list, edit, package)
switches on `--json`; the outcome commands print the bare lead id.

**Recommendation:** pick one contract and document it in §8. Suggested:
`mark` respects `--json` (JSON when set; a one-line
`marked <prefix> <mark>` otherwise), outcome commands keep the bare lead
id (script-friendly). **Effort: S.**

---

## Low severity

### L1. The ideological gate compiles its regex per red line per posting

**Evidence:** `src/domain/gates.rs:122` — `regex::Regex::new(...)` inside
the `evaluate` loop. Every other regex in the codebase is a
compile-once `LazyLock` static (`src/ingest/extract.rs:17-59`,
`src/ingest/platforms.rs:99`).

**Recommendation:** impact is trivial (config-rare red lines, few
postings), but the codebase convention is compile-once. Either accept
with a comment saying why, or compile the patterns once per config (e.g.
in `Config::validate`, storing them alongside). **Effort: S.**

### L2. `execute_applied` resolves the lead prefix twice

**Evidence:** `src/commands/mod.rs:1193` calls
`select_lead(&projection, &args.lead)` for the method default, then
`record_outcome` (`1175`) calls `select_lead` again with the same
prefix. Two lookups and two possible error messages for one user
mistake.

**Recommendation:** pass the resolved record (or `lead_id`) into
`record_outcome` instead of the prefix. **Effort: S.**

### L3. `IngestKind` → string mapping is ad hoc

**Evidence:** `src/commands/mod.rs:202-206` maps `IngestKind` to
`&'static str` inline while constructing `IngestSummary`.

**Recommendation:** an `as_str()` (or `Display`) on `IngestKind` next to
the enum in `domain/lead.rs`. **Effort: S.**

### L4. Stale increment-tense module docs

**Evidence:**

- `src/domain/lead.rs:4-6` — "Gates and scoring land in Increments 2–3;
  the aggregate already understands `reviewed` marks so the
  durable-ignore suppression rule is in place before marks exist" —
  both clauses describe a past plan.
- `src/resume.rs:6-7` — "v0 consumes `skills` (scoring, Increment 3)
  and `basics`/`work` (the apply-package cheat sheet, Increment 4a)" —
  same family; reads as a plan rather than a description.

**Recommendation:** rewrite in present tense describing what the module
does today. **Effort: S.**

### L5. `--start-date`/`--reason` validity is runtime-checked

**Evidence:** `src/commands/mod.rs:1285-1290` bails at runtime for
`--start-date` on non-`accepted` and `--reason` on non-`archived`.
Clap-level conditional requires on an enum positional are awkward; the
runtime check is clear and tested.

**Recommendation:** note only — keep as is unless a natural clap
formulation appears. **Effort: S** if ever done.

### L6. Unresolved "probably temporary" decision marker in `cli.rs`

**Evidence:** `src/cli.rs:390` — "(Probably) temporary until I decide
what the default command should do." on `arg_required_else_help`.

**Recommendation:** resolve the default-command question (or file a
pebble for it) and update the comment; decision debt shouldn't live in
code comments. **Effort: S** (the decision; no code change until made).

### L7. Test coverage gaps

**Evidence (verified by reading the test modules):**

- `review_loop` / `read_review_key` / `RawModeGuard`
  (`src/commands/mod.rs:868-999`): zero tests. The mark paths it calls
  are tested via `mark_lead`, but the `m`-key resume-degradation branch
  (`956-964`) is untested.
- `filter_events` (`1331-1350`): the `--type` filter path has no direct
  test (the events tests cover lead-prefix resolution only,
  `1941-1986`).
- `queue_entry`/`QueueEntry` (`304-316`): no direct test of the JSON row
  shape.

**Recommendation:** add a `filter_events` type-filter test and extract
the `m`-key cheat-sheet degradation into a testable function (both S).
Leave the raw-mode key reading untested — real terminal I/O; a pty
harness isn't worth it at this scale. **Effort: S.**

### L8. 12 of 14 `execute_*` functions are `async` with no `await` inside

**Evidence:** only `execute_ingest` awaits network I/O
(`src/commands/mod.rs:65`); `execute_completion` is already sync
(`788`). The other twelve (`show`, `applied`, `screened`,
`interviewed`, `offered`, `outcome`, `events`, `list`, `mark`, `edit`,
`package`, `review`) contain no `.await`.

**Recommendation:** make them plain `fn`s; `cli::execute` stays `async`
and calls them (it already does this for `execute_completion`,
`src/cli.rs:471`). Removes async ceremony from commands with no async
I/O. Arguable — uniform signatures have value; decide during H1.
**Effort: S.**

### L9. Double projection rebuild after append in `ingest` and `edit`

**Evidence:** `execute_ingest` rebuilds at `src/commands/mod.rs:81-82`,
appends, then rebuilds again at `93-94` for the card; `execute_edit`
likewise `436-437` then `458-459`. The comments say this is deliberate
("Rebuild the projection to get the updated lead record").

**Recommendation:** accept at v0 volume (say so in the comment), or have
`record_ingest`/`record_edit` return the updated `LeadRecord`.
**Effort: S.**

### L10. `docs/decisions/README.md` example filename doesn't exist

**Evidence:** `docs/decisions/README.md:4` — "(e.g.,
`0001-jsonl-event-log-over-sqlite.md`)" — the actual decision 0001 is
`0001-fresh-start-over-alpha-csv-import.md`.

**Recommendation:** fix the example to the real file. **Effort: S.**

### L11. `ingest`'s "exactly one of" error arm is unreachable

**Evidence:** `src/commands/mod.rs:73` errors when neither or both of
`<url>`/`--file` are set, but clap already enforces
`required_unless_present` + `conflicts_with` (`src/cli.rs:17`).

**Recommendation:** note only — the defensive arm is cheap and harmless;
keep, optionally with a comment that clap guards it. **Effort: S.**

---

## History notes (no action)

### PR #17 was closed unmerged, but its commits are on main

`9bad2fa` ("Finish the v0 command surface: package + completion") and
`74a0b86` ("Address PR #17 review feedback") are in main's linear
history (verified: `git branch -r --contains 9bad2fa` includes
`origin/main`), while PR #17 itself was closed unmerged. The work
landed; only the PR record diverges from the commit history. Nothing to
fix — recorded here so future archaeology (blame → PR link → "closed")
isn't confusing.

---

## Checked and deliberately fine (non-findings)

- **unwrap/expect discipline:** zero non-test `unwrap()`s in commands,
  projections, the JSONL store, and config. The only production
  `expect()`s are on constant regexes/URLs that cannot fail
  (`src/ingest/platforms.rs:99`, `src/ingest/mod.rs:335`).
- **Error handling:** miette throughout the command layer;
  `thiserror` appears only for `FetchError`, which is correct (a typed
  transport error checked via `downcast_ref`).
- **Log/stdout discipline:** logs go to the file sink only (decision
  0005); stdout is command output — holds everywhere.
- **Full-log re-reads per `append`/`load`** (`src/event_store/jsonl.rs:168`,
  `228-234`): accepted at v0 volume; SQLite remains the documented §7
  escape hatch.
- **Config defaults** match their decision records: log level `error`,
  telemetry off, equal scoring weights, `remote_only` false,
  `reject_location_only` false (documented rationale at
  `src/config.rs:62-66`).
- **`#[allow(clippy::too_many_arguments)]`** on `decide_ingest`/`decide_edit`
  (`src/domain/lead.rs:116`, `216`): justified in-comment at the
  decide/evolve seam.
- **Test suite:** 318 passing, partitioned with `// ── section ──`
  comments per AGENTS.md, deterministic, no network (scripted
  `Fetcher`s, `cfg(test)` `open_url` stub).

---

## Suggested execution order

1. **Doc fixes:** M1, M2, M3, L10 — one PR, no code risk. (S)
2. **Comment/code hygiene:** M4, M5, M6-description, L4 — one PR. (S)
3. **Boilerplate helpers:** H2 + H3 — one PR, mechanical, lands before
   the split so H1 is pure moves. (S)
4. **The split:** H1 (with L8 decided alongside) — one PR. (M)
5. **Small consistency:** M7, M8, M9, L2, L3 — one PR. (S)
6. **UX + docs + tests:** M6-README, M10, L1, L7 — one PR. (S–M)
