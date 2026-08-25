# 0001: Event Schema and Command Surface

Status: proposed (Increment 0, GWLJ-rk2cnb)
Spec: `docs/specs/job-search-automation-v2.md`

This document defines the event-sourced model and CLI command surface for
`gwl-jobs` v0. It is the design contract for the pipeline increments that
follow. It answers the four Increment 0 questions:

1. **Lead identity** — what uniquely identifies a lead, and re-ingest
   semantics for updated/reposted leads (§2).
2. **Full lifecycle in the event schema** — including outcome events v0's
   pipeline does not emit (§3).
3. **The alpha's fate** — fresh start vs. one-time CSV import (§6).
4. **Review-queue interaction model** — prompt loop vs. TUI (§5).

It supersedes the placeholder `lead open|list|close` subcommands, which are
removed when the first pipeline increment lands.

**Review-round decisions:** two naming/scope changes from design review are
recorded as decision records —
`docs/decisions/0001-fresh-start-over-alpha-csv-import.md` (question (c) is
answered **fresh start**; no import) and `docs/decisions/0002-v0-naming.md`
(the `no-action` mark is `defer`, event types carry no `lead_` prefix, the
queue command is `list`). The spec has been updated to match.

## 1. Model overview

- **Aggregate:** `Lead`. One event stream per lead: `lead/<lead_id>`.
- **Source of truth:** append-only JSONL event log at
  `<data_dir>/events.jsonl` (one envelope per line). Never rewritten; schema
  evolution happens via upcasting at read time (§4).
- **Reads:** in-memory projections rebuilt from the log on startup (§7).
  SQLite remains deferred.
- **Writes:** commands are pure `decide(command, state) -> events` functions
  over the aggregate's replayed state; `evolve(state, event) -> state` folds
  events. The `EventStore` trait is the only I/O seam:
  `append(stream, expected_seq, events)` and `load(stream)` / `replay()`.

### Durability contract

- **Single writer.** A command holds an exclusive lock (lockfile in the data
  dir, `flock`) for the whole load → decide → append cycle; a second
  concurrent process fails fast rather than racing `expected_seq`.
- **A batch append is the commit unit.** All events in one `append()` call
  are serialized and written with a single `write(2)` on an `O_APPEND` file
  descriptor, followed by `fsync`. The two-event `reviewed` + `apply_queued`
  batch (§5) therefore cannot be split by anything short of a crash
  mid-write.
- **Crash recovery.** On replay, a malformed trailing line (a torn write) is
  discarded with a warning; any other malformed line is a hard error. A batch
  torn between its events can leave a prefix visible — the §7
  pending-recovery rule (an `apply-automatically` mark with no subsequent
  `apply_queued` stays pending) is the semantic backstop.

### Event envelope

Every line in the log is one envelope:

```json
{
  "envelope_version": 1,
  "id": "0192f8a1-…",
  "stream": "lead/0192f89f-…",
  "seq": 3,
  "type": "scored",
  "schema_version": 1,
  "occurred_at": "2026-08-25T19:00:00Z",
  "recorded_at": "2026-08-25T19:00:01Z",
  "causation_id": "0192f8a0-…",
  "correlation_id": "0192f89e-…",
  "payload": { "…": "…" }
}
```

| Field              | Meaning                                                              |
| ------------------ | -------------------------------------------------------------------- |
| `envelope_version` | Version of the envelope shape itself (§4).                           |
| `id`               | UUIDv7, unique per event.                                            |
| `stream`           | `lead/<lead_id>`; the aggregate stream.                              |
| `seq`              | Per-stream sequence, 1-based, gapless. Optimistic concurrency.       |
| `type`             | Event type (§3).                                                     |
| `schema_version`   | Version of this event type's payload shape (§4).                     |
| `occurred_at`      | When the thing happened (user-supplied for retro-recorded outcomes). |
| `recorded_at`      | When the event was appended.                                         |
| `causation_id`     | ID of the event that directly caused this one, if any.               |
| `correlation_id`   | One UUID per command invocation; groups a command's events.          |
| `payload`          | Event-type-specific body.                                            |

`recorded_at` - `occurred_at` is meaningful for retro-recorded outcomes;
for pipeline events they are near-identical.

## 2. Lead identity and re-ingest semantics (question a)

A lead has two identities:

- **`lead_id`** — a UUIDv7 minted at first ingest. This is the durable
  internal identity and the stream ID. It survives reposts, URL changes, and
  edited postings.
- **Dedupe key** — a deterministic, derived matcher computed at every
  ingest, used to decide whether an incoming posting is new or a re-ingest
  of an existing lead.

### Dedupe key precedence

Computed from extracted fields, first applicable rule wins:

1. **`req:<company-slug>:<req-id>`** — when both company and req id were
   extracted. Normalized: lowercase, trimmed, whitespace collapsed.
2. **`url:<canonical-url>`** — canonicalized: scheme/host lowercased,
   default port dropped, fragment dropped, _known tracking parameters_
   dropped (`utm_*`, `fbclid`, `gclid`, …), all other query params
   **preserved**, trailing slashes collapsed. Boards that put the job id in
   the query (`?jobId=123`) must not have distinct postings collapse into
   one key; per-adapter rules may drop additional params only when the
   board's URL contract is known. Known risk: a board variant that puts a
   session/token (rather than the job id) in a non-tracking param defeats
   dedupe for that board until its adapter lands.
3. **`tc:<sha256>`** — SHA-256 over `normalize(title) + "\n" +
normalize(company)`. Fallback for file drops with no URL or req id.

All available identifier forms (req, url, title-company) are stored in the
event payload and indexed by the projection, regardless of which form became
the dedupe key. A re-ingest matches an existing lead on the **strongest form
the incoming posting carries**: `req:` and `url:` hits always match (checked
in precedence order — so a posting that gains a req id on repost still
matches the earlier URL-keyed ingest via the URL index). The `tc:` fallback
is consulted **only when the incoming posting has neither a req id nor a
URL** — a posting carrying a strong identifier never merges onto another
lead on title+company alone.

### Re-ingest semantics

On ingest, compute identifiers and consult the lead index:

- **No match** → mint `lead_id`, emit `ingested`, run gates, then
  scoring.
- **Match, latest mark is `ignore`** → the ignore mark is **durable**: emit
  `reingest_suppressed` (audit trail) and stop. No re-gating, no
  re-scoring, no queue re-entry. A re-ingested ignored lead never re-enters
  the queue, on this run or any future run. Because marks are latest-wins,
  re-marking an ignored lead lifts suppression: the next re-ingest follows
  the ordinary `updated` path.
- **Match, otherwise** → append `updated` to the existing stream (new
  extraction snapshot + list of changed fields), then **re-run gates and
  scoring on the new content**: a gate failure emits `rejected` (new
  revision); a pass emits `scored` (new revision). Rationale: gates are
  content-dependent — a repost that now lists compensation may pass a floor
  it previously failed, and vice versa. Whether the lead re-enters the
  pending queue is governed by §7: only leads whose latest mark is absent or
  `defer`, and which carry no outcome, re-enter. A lead already marked
  `apply-automatically`/`apply-manual`, or one with an outcome, gets the
  refreshed snapshot and new score revision but stays off the pending queue.
  Its deferral count is retained (it is history, and the user can see it in
  the queue).

Known v0 limitation: two genuinely different postings that both lack a URL
and a req id can still collide on the title+company hash, and a `tc:`-only
drop can merge onto a lead that merely shares its title and company. These
cases are ambiguous by inspection; acceptable at this volume. The converse
does not self-heal: once a lead is keyed `tc:`-only, a later URL-bearing
repost of the same posting mints a new lead (strong forms match first), so
duplicates of that kind are tolerated until source adapters land.

## 3. Event schema (question b)

`●` = emitted by the v0 pipeline. `○` = defined in the schema now, emitted
only by `gwl-jobs outcome` (user-recorded) — the pipeline itself never
produces them in v0.

### Pipeline events

**`ingested`** ● — first sighting of a lead.

```json
{
  "dedupe_key": "req:nvidia:JR2018233",
  "identifiers": {
    "req": "req:nvidia:JR2018233",
    "url": "url:https://nvidia.wd5.myworkdayjobs.com/en-US/NVIDIAExternalCareerSite/job/…",
    "tc": "tc:9f2c…"
  },
  "source": "drop-in",
  "url": "https://nvidia.wd5.myworkdayjobs.com/…",
  "raw_text": "…full extracted posting text…",
  "extracted": {
    "title": "Principal Software Engineer: DGX Cloud Production Engineering",
    "company": "NVIDIA",
    "req_id": "JR2018233",
    "location": "Remote, US",
    "remote": true,
    "comp": {
      "min": 220000,
      "max": 290000,
      "currency": "USD",
      "period": "year",
      "raw": "$220,000 - $290,000"
    }
  }
}
```

`extracted` fields are best-effort; any may be absent. `raw_text` is stored
in the log (it is the source of truth; volume is trivial at this scale).

**`updated`** ● — re-ingest matched an existing, non-ignored lead.

```json
{
  "dedupe_key": "req:nvidia:JR2018233",
  "identifiers": { "…": "…" },
  "changed": ["comp", "location"],
  "raw_text": "…",
  "extracted": { "…": "…" }
}
```

**`reingest_suppressed`** ● — re-ingest matched a durably ignored lead.

```json
{ "dedupe_key": "tc:9f2c…", "suppressed_by_mark": "ignore" }
```

**`rejected`** ● — a hard gate failed.

```json
{
  "gate": "compensation-floor",
  "reason": "quoted max $140,000 below floor $180,000",
  "revision": 2
}
```

`gate` ∈ `remote-only | compensation-floor | blacklist | ideological`. The
`ideological` mechanism (a filter list) exists in v0 with empty content; the
LLM scorer that fills it is vNext. `revision` counts gate/score evaluations
of this lead (re-ingests re-evaluate).

**`scored`** ● — survived all gates.

```json
{
  "composite": 75,
  "revision": 1,
  "dimensions": [
    { "name": "level", "score": 80, "weight": 0.3, "confidence": 1.0 },
    { "name": "skills", "score": 90, "weight": 0.3, "confidence": 1.0 },
    {
      "name": "compensation",
      "score": 60,
      "weight": 0.4,
      "confidence": 1.0
    }
  ],
  "breakdown": "75 = 0.3·level(80) + 0.3·skills(90) + 0.4·compensation(60)"
}
```

Per spec: unknown comp passes the floor gate; the compensation dimension is
then omitted from `dimensions`, weights renormalize over the remainder, and
`breakdown` notes it (e.g. `82 = 0.5·level(80) + 0.5·skills(84)
[compensation: unknown, weight renormalized]`). `confidence` defaults to
`1.0` and is only meaningful once LLM scorers arrive.

**`reviewed`** ● — the user marked a lead. Marks are latest-wins;
re-marking emits a new event.

```json
{ "mark": "apply-automatically", "note": null }
```

`mark` ∈ `apply-automatically | apply-manual | defer | ignore`.
`defer` increments the projected deferral count. `ignore` is durable
(§2).

**`apply_queued`** ● — apply package prepared for an
`apply-automatically` lead.

```json
{
  "package": {
    "cover_letter_path": "~/.config/gwl-jobs/generic-letter.pdf",
    "resume_path": "~/Source/resume/resume.pdf",
    "cheat_sheet": [
      { "question": "Years of experience with Kubernetes?", "answer": "6" }
    ]
  },
  "url": "https://boards.example.com/apply/…"
}
```

### Outcome events (○ — schema now, pipeline never emits)

Recorded by the user via `gwl-jobs outcome`. Each accepts optional `note`;
`occurred_at` may be user-supplied for retro recording.

| Type                   | Payload extras                    | Terminal? |
| ---------------------- | --------------------------------- | --------- |
| `applied`              | `method: manual \| auto-assisted` | no        |
| `screened`             | `contact?`                        | no        |
| `interviewed`          | `stage?`                          | no        |
| `offered`              | —                                 | no        |
| `accepted`             | `start_date?`                     | yes       |
| `rejected_by_employer` | —                                 | yes       |
| `withdrawn`            | —                                 | yes       |
| `declined`             | — (user declined an offer)        | yes       |
| `archived`             | `reason`                          | yes       |

A lead with any outcome event leaves the review queue (§7). This set is
deliberately closed-ended rather than a free-form `outcome {kind}` so
that projections and future analytics can match on concrete types.

## 4. Schema versioning and upcasting

Every event type starts at `schema_version: 1`.

- **Additive changes** (new optional payload field) do not bump the version;
  readers must tolerate unknown fields.
- **Anything else** (rename, remove, type change, semantic change) bumps the
  version and requires a registered **upcaster**:
  `upcast(type, from_version, payload_json) -> payload_json`, chained at
  replay (`v1 → v2 → v3`).
- The log is **never rewritten**. Upcasting happens in memory on read.
- The envelope carries an explicit `envelope_version`, versioned
  independently of payload `schema_version`; envelope changes follow the
  same additive/upcaster rules.

This is the plan required before the first real event is written; the first
pipeline increment ships the upcaster registry (empty) alongside the log
reader so the seam exists from day one.

## 5. Review-queue interaction model (question d)

**Decision: a blocking prompt loop. No TUI in v0.**

`gwl-jobs review`:

1. Rebuilds the projection, prints the ranked queue (rank, composite,
   title @ company, deferral count).
2. Steps through pending leads highest-score-first. For each: prints title,
   company, location, comp, URL, composite score with human-readable
   breakdown, and deferral count; then prompts:

   ```
   [a] apply-automatically  [m] apply-manual  [d] defer  [i] ignore  [s] skip  [q] quit
   ```

   - `a` — first prepares the package (generic letter + cheat sheet +
     resume PDF); on success, appends `reviewed{apply-automatically}` and
     `apply_queued` **in a single batch `append()`** (one `correlation_id`;
     the `apply_queued`'s `causation_id` is the `reviewed` event's id), then
     opens the posting URL. If package preparation fails, no events are
     appended and the lead stays pending. The mark _is_ the approval; there
     is no second confirmation. The browser click is the user's.
   - `m` — emits `reviewed{apply-manual}`; prints JD + resume context +
     cheat sheet for the user to act on.
   - `d` — emits `reviewed{defer}`; deferral count +1; reappears next
     review.
   - `i` — emits `reviewed{ignore}`; durable (§2).
   - `s` — no event; move on. Skip is session-local with no durable
     effect: a skipped lead is indistinguishable from an un-reviewed one
     and reappears in the same position next review (unlike `d`, no
     deferral counted).
   - `q` — stop.

3. Because state is the event log, quitting mid-loop loses nothing; the loop
   is resumable by construction.

Prompt input reads raw keys via `console::Term::read_key` (the lower-level
crate `dialoguer` itself builds on — small, pure-Rust). `dialoguer::Select`
was considered and rejected: it navigates with arrows + Enter and cannot
deliver the single-key `a`/`m`/`d`/`i`/`s`/`q` hotkeys above, which are the
point — the review loop is the screen the user touches most, and one-key
marks are materially cheaper than navigate-and-confirm (the project's
energy-economics goal). A ratatui TUI is explicitly vNext-sized and out of
scope.

## 6. The alpha's fate (question c)

**Decision: fresh start. The alpha CSV is not imported.**

`~/Documents/Job Hunt/events/events.csv` records 11 real outcomes (`Applied`
×8, `Screened` ×3) from the manual alpha. The countervailing value of
importing was dedupe against the past — without it, a reposted req for an
already-applied job (e.g. NVIDIA JR2018233) can re-enter the review queue as
if new. **That risk is accepted, explicitly.** The remedy is manual and
cheap: the §3 outcome set subsumes the alpha's Applied/Screened vocabulary,
so any historical row can be retro-recorded with `gwl-jobs outcome` (`--at`
carries the original date) if a familiar posting resurfaces. The alpha's
`jds/` archive and the cover-letter corpus stay on disk as reference
material; nothing in the tool reads them.

## 7. Projections and queue membership

In-memory, rebuilt by replaying the log at startup:

- **LeadIndex** — every identifier form → `lead_id` (drives §2 matching).
- **LeadBook** — per lead: latest extraction snapshot, latest score
  (max revision), latest mark, deferral count, gate status, outcome state.

**Review queue membership:** leads with a current `scored`, no
subsequent failing `rejected`, no outcome event, not archived, and
latest mark absent or `defer`. Ranked by composite score descending.
`apply-automatically`/`apply-manual` leads that have not yet recorded an
outcome appear in `gwl-jobs list --all` but not in the pending queue —
they have been acted on; `gwl-jobs outcome` is how they move forward.

**Pending recovery:** a lead whose latest mark is `apply-automatically` but
which has **no** subsequent `apply_queued` is treated as still-pending. That
state is only reachable via a crash mid-batch (§1); this rule is the
recovery path.

## 8. Command surface

Supersedes and removes the placeholder `lead open|list|close`.

| Command                                               | Purpose                                                                                                                                      | Events emitted                                                              |
| ----------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------- |
| `gwl-jobs ingest <url>`                               | Fetch, extract, dedupe, gate, score a posting.                                                                                               | `ingested` / `updated` / `reingest_suppressed`, then `rejected` or `scored` |
| `gwl-jobs ingest --file <path>`                       | Same, from a local HTML/PDF/text drop.                                                                                                       | same                                                                        |
| `gwl-jobs list [--all]`                               | Print the ranked review queue.                                                                                                               | —                                                                           |
| `gwl-jobs review`                                     | Interactive prompt loop (§5).                                                                                                                | `reviewed`, `apply_queued`                                                  |
| `gwl-jobs mark <lead> <mark> [--note]`                | Non-interactive mark (scriptable). `apply-automatically` runs the same prepare → batch-append → open flow as the review loop's `a` key (§5). | `reviewed` (+ `apply_queued`)                                               |
| `gwl-jobs package <lead>`                             | (Re)build the apply package for a lead marked `apply-automatically`; re-print and re-open the URL. Bails on unmarked leads — mark first.     | `apply_queued`                                                              |
| `gwl-jobs show <lead>`                                | Full detail: snapshot, score history, marks, events.                                                                                         | —                                                                           |
| `gwl-jobs outcome <lead> <type> [--note] [--at <ts>]` | Record an outcome (§3 table).                                                                                                                | `applied` / `screened` / …                                                  |
| `gwl-jobs events [--lead <id>] [--type <t>]`          | Dump/filter the raw log (debugging, golden tests).                                                                                           | —                                                                           |
| `gwl-jobs completion`                                 | Shell completions (existing).                                                                                                                | —                                                                           |

`<lead>` addressing: unambiguous UUID prefix of the `lead_id`. The review
loop needs no addressing. Conventions carried forward: clap subcommands,
miette errors (unimplemented commands `bail!` loudly), tracing with logs on
stderr and command output on stdout.

## 9. Module layout

The flat modules evolve toward the event-sourcing seams as pipeline
increments land:

```
src/
  cli.rs            # clap surface (§8)
  commands/         # one module per command; thin, calls domain
  domain/
    lead.rs         # aggregate: decide() / evolve()
    events.rs       # envelope + event payloads + schema versions
    identity.rs     # dedupe key computation, canonicalization
    gates.rs        # hard filters
    scoring.rs      # dimensions + composite + breakdown rendering
  event_store/
    mod.rs          # EventStore trait
    jsonl.rs        # append-only JSONL implementation
    upcast.rs       # upcaster registry
  projections/      # LeadIndex, LeadBook, queue views
  ingest/           # source adapters (v0: drop-in)
  config.rs         # TOML config + AppPaths
  telemetry.rs      # as-is
```

`src/main.rs` stays thin: parse CLI, init telemetry, dispatch, shutdown.

## 10. Testing posture

Per increment, per AGENTS.md: unit tests for gate/scoring/identity logic
(canonicalization, precedence, re-ingest matching, weight renormalization,
breakdown rendering) and golden round-trip tests for the JSONL log
(write → replay → projection equality, upcaster no-op at current versions,
torn-tail replay tolerance).
