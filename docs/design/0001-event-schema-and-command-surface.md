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

### Event envelope

Every line in the log is one envelope:

```json
{
  "id": "0192f8a1-…",
  "stream": "lead/0192f89f-…",
  "seq": 3,
  "type": "lead_scored",
  "schema_version": 1,
  "occurred_at": "2026-08-25T19:00:00Z",
  "recorded_at": "2026-08-25T19:00:01Z",
  "causation_id": "0192f8a0-…",
  "correlation_id": "0192f89e-…",
  "payload": { "…": "…" }
}
```

| Field            | Meaning                                                        |
| ---------------- | -------------------------------------------------------------- |
| `id`             | UUIDv7, unique per event.                                      |
| `stream`         | `lead/<lead_id>`; the aggregate stream.                        |
| `seq`            | Per-stream sequence, 1-based, gapless. Optimistic concurrency. |
| `type`           | Event type (§3).                                               |
| `schema_version` | Version of this event type's payload shape (§4).               |
| `occurred_at`    | When the thing happened (user-supplied for imports/outcomes).  |
| `recorded_at`    | When the event was appended.                                   |
| `causation_id`   | ID of the event that directly caused this one, if any.         |
| `correlation_id` | One UUID per command invocation; groups a command's events.    |
| `payload`        | Event-type-specific body.                                      |

`recorded_at` - `occurred_at` is meaningful for imports and retro-recorded
outcomes; for pipeline events they are near-identical.

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
   default port dropped, fragment dropped, all query params dropped
   (drop-in adapter v0; per-adapter allowlists may retain params later),
   trailing slashes collapsed.
3. **`tc:<sha256>`** — SHA-256 over `normalize(title) + "\n" +
normalize(company)`. Fallback for file drops with no URL or req id.

All available identifier forms (req, url, title-company) are stored in the
event payload and indexed by the projection, regardless of which form became
the dedupe key. A re-ingest matches an existing lead if **any** indexed form
hits, checked in precedence order — so a posting that gains a req id on
repost still matches the earlier URL-keyed ingest via the URL index.

### Re-ingest semantics

On ingest, compute identifiers and consult the lead index:

- **No match** → mint `lead_id`, emit `lead_ingested`, run gates, then
  scoring.
- **Match, latest mark is `ignore`** → the ignore mark is **durable**: emit
  `lead_reingest_suppressed` (audit trail) and stop. No re-gating, no
  re-scoring, no queue re-entry. A re-ingested ignored lead never re-enters
  the queue, on this run or any future run.
- **Match, otherwise** → append `lead_updated` to the existing stream (new
  extraction snapshot + list of changed fields), then **re-run gates and
  scoring on the new content**: a gate failure emits `lead_rejected` (new
  revision); a pass emits `lead_scored` (new revision). Rationale: gates are
  content-dependent — a repost that now lists compensation may pass a floor
  it previously failed, and vice versa. The lead re-enters/updates the queue
  at its new score; its deferral count is retained (it is history, and the
  user can see it in the queue).

Known v0 limitation: two genuinely different postings with no URL and no req
id can collide on the title+company hash. Acceptable at this volume; a later
URL-form match self-heals by merging onto the existing stream.

## 3. Event schema (question b)

`●` = emitted by the v0 pipeline. `○` = defined in the schema now, emitted
only by `gwl-jobs outcome` (user-recorded) or `import-alpha` — the pipeline
itself never produces them in v0.

### Pipeline events

**`lead_ingested`** ● — first sighting of a lead.

```json
{
  "dedupe_key": "req:nvidia:JR2018233",
  "identifiers": {
    "req": "req:nvidia:JR2018233",
    "url": "url:https://nvidia.wd5.myworkdayjobs.com/en-US/NVIDIAExternalCareerSite/job/…"
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
in the log (it is the source of truth; volume is trivial at this scale). For
imports without posting text, `raw_text` is omitted.

**`lead_updated`** ● — re-ingest matched an existing, non-ignored lead.

```json
{
  "dedupe_key": "req:nvidia:JR2018233",
  "identifiers": { "…": "…" },
  "changed": ["comp", "location"],
  "raw_text": "…",
  "extracted": { "…": "…" }
}
```

**`lead_reingest_suppressed`** ● — re-ingest matched a durably ignored lead.

```json
{ "dedupe_key": "tc:9f2c…", "suppressed_by_mark": "ignore" }
```

**`lead_rejected`** ● — a hard gate failed.

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

**`lead_scored`** ● — survived all gates.

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

**`lead_reviewed`** ● — the user marked a lead. Marks are latest-wins;
re-marking emits a new event.

```json
{ "mark": "apply-automatically", "note": null }
```

`mark` ∈ `apply-automatically | apply-manual | no-action | ignore`.
`no-action` increments the projected deferral count. `ignore` is durable
(§2).

**`lead_apply_queued`** ● — apply package prepared for an
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

Recorded by the user via `gwl-jobs outcome` or produced by the alpha import.
Each accepts optional `note`; `occurred_at` may be user-supplied for retro
recording.

| Type                        | Payload extras                    | Terminal? |
| --------------------------- | --------------------------------- | --------- |
| `lead_applied`              | `method: manual \| auto-assisted` | no        |
| `lead_screened`             | `contact?`                        | no        |
| `lead_interviewed`          | `stage?`                          | no        |
| `lead_offered`              | —                                 | no        |
| `lead_rejected_by_employer` | —                                 | yes       |
| `lead_withdrawn`            | —                                 | yes       |
| `lead_declined`             | — (user declined an offer)        | yes       |
| `lead_archived`             | `reason`                          | yes       |

A lead with any outcome event leaves the review queue (§7). This set is
deliberately closed-ended rather than a free-form `lead_outcome {kind}` so
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
- The envelope itself carries an implicit version 1; envelope changes follow
  the same rule.

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
   [a] apply-automatically  [m] apply-manual  [n] no-action  [i] ignore  [s] skip  [q] quit
   ```

   - `a` — emits `lead_reviewed{apply-automatically}`, then immediately
     prepares the package (generic letter + cheat sheet + resume PDF), emits
     `lead_apply_queued`, and opens the posting URL. The mark _is_ the
     approval; there is no second confirmation. The browser click is the
     user's.
   - `m` — emits `lead_reviewed{apply-manual}`; prints JD + resume context +
     cheat sheet for the user to act on.
   - `n` — emits `lead_reviewed{no-action}`; deferral count +1; reappears
     next review.
   - `i` — emits `lead_reviewed{ignore}`; durable (§2).
   - `s` — no event; move on (differs from `n`: no deferral counted).
   - `q` — stop.

3. Because state is the event log, quitting mid-loop loses nothing; the loop
   is resumable by construction.

Prompt input uses `dialoguer` (small, pure-Rust, single-key selection) —
justified by the project's energy-economics goal: the review loop is the
screen the user touches most, and arrow/one-key selection is materially
cheaper than typed answers. A ratatui TUI is explicitly vNext-sized and out
of scope.

## 6. The alpha's fate (question c)

**Decision: one-time import, not a fresh start.**

`~/Documents/Job Hunt/events/events.csv` records 11 real outcomes (`Applied`
×8, `Screened` ×3) with company, role, req id, URL, source, contact, and
notes. A fresh start would lose the one thing this history is good for:
**dedupe against the past.** Without it, a reposted NVIDIA JR2018233 would
cheerfully re-enter the review queue for a job already applied to. The
outcome events in §3 exist partly so this import is lossless.

A later small increment adds `gwl-jobs import-alpha <csv>`:

- Each row emits `lead_ingested` (`source: "alpha-csv"`, `occurred_at` from
  the Date column, extracted fields from the columns, no `raw_text`) plus
  exactly one outcome event: `Applied` → `lead_applied{method: manual}`,
  `Screened` → `lead_screened{contact}`, carrying notes.
- Gates and scoring are **not** run — the import is a historical record, not
  a triage candidate, and there is no JD text to score.
- Imported leads are terminally out of the review queue by the §7 rule (any
  outcome event ⇒ off-queue).
- The import is idempotent via dedupe keys: re-running it matches the
  already-imported leads and appends nothing.

The import is not on the v0 critical path; it lands after the pipeline as
its own increment. If it were cut entirely, the fallback is a fresh start —
accepted with eyes open that re-apply protection is lost.

## 7. Projections and queue membership

In-memory, rebuilt by replaying the log at startup:

- **LeadIndex** — every identifier form → `lead_id` (drives §2 matching).
- **LeadBook** — per lead: latest extraction snapshot, latest score
  (max revision), latest mark, deferral count, gate status, outcome state.

**Review queue membership:** leads with a current `lead_scored`, no
subsequent failing `lead_rejected`, no outcome event, not archived, and
latest mark absent or `no-action`. Ranked by composite score descending.
`apply-automatically`/`apply-manual` leads that have not yet recorded an
outcome appear in `gwl-jobs queue --all` but not in the pending queue —
they have been acted on; `gwl-jobs outcome` is how they move forward.

## 8. Command surface

Supersedes and removes the placeholder `lead open|list|close`.

| Command                                               | Purpose                                              | Events emitted                                                                                       |
| ----------------------------------------------------- | ---------------------------------------------------- | ---------------------------------------------------------------------------------------------------- |
| `gwl-jobs ingest <url>`                               | Fetch, extract, dedupe, gate, score a posting.       | `lead_ingested` / `lead_updated` / `lead_reingest_suppressed`, then `lead_rejected` or `lead_scored` |
| `gwl-jobs ingest --file <path>`                       | Same, from a local HTML/PDF/text drop.               | same                                                                                                 |
| `gwl-jobs queue [--all]`                              | Print the ranked review queue.                       | —                                                                                                    |
| `gwl-jobs review`                                     | Interactive prompt loop (§5).                        | `lead_reviewed`, `lead_apply_queued`                                                                 |
| `gwl-jobs mark <lead> <mark> [--note]`                | Non-interactive mark (scriptable).                   | `lead_reviewed`                                                                                      |
| `gwl-jobs package <lead>`                             | (Re)build the apply package; open the posting URL.   | `lead_apply_queued`                                                                                  |
| `gwl-jobs show <lead>`                                | Full detail: snapshot, score history, marks, events. | —                                                                                                    |
| `gwl-jobs outcome <lead> <type> [--note] [--at <ts>]` | Record an outcome (§3 table).                        | `lead_applied` / `lead_screened` / …                                                                 |
| `gwl-jobs events [--lead <id>] [--type <t>]`          | Dump/filter the raw log (debugging, golden tests).   | —                                                                                                    |
| `gwl-jobs import-alpha <csv>`                         | One-time alpha import (§6; later increment).         | `lead_ingested` + outcome events                                                                     |
| `gwl-jobs completion`                                 | Shell completions (existing).                        | —                                                                                                    |

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
(write → replay → projection equality, upcaster no-op at current versions).
