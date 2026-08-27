# Job Search Automation — Refined Prompt (v2)

Build a job-search triage and application-assist CLI in Rust, extending the
existing `gwl-job-tools` crate. This document is the specification. Where it
conflicts with your instincts, ask before deviating.

## Problem Statement

The core problem is **energy economics**, not "find a job." I have a fixed,
small daily energy budget (autoimmune disorder) and experience overwhelm from
the volume of postings (anxiety/autism). The job-search process spends that
budget on two things that drain me fastest:

1. sifting through a high volume of postings, and
2. repetitively typing information that is already in my resume.

Competitors are automating their applications, so manual speed is also a
competitive disadvantage.

The tool's job is to **convert my scarce energy into quality applications** by
automating the high-volume, low-judgment work (sift, score, capture metadata,
and repetitive typing) and reserving my judgment for the high-value decisions
(which jobs to pursue). The human-in-the-loop review gate is the whole point — this is a
triage assistant, not a fire-and-forget bot.

## What It Should Be/Do (v0)

A deterministic pipeline with these stages:

### 1. Ingest

Accept a URL or a file (HTML/PDF/text; PDF support is deferred to
GWLJ-mxyp63, which also covers headless print-to-PDF for JS-rendered
sites). Fetch and extract the main text. Emit a
`ingested` event containing the raw text plus best-effort structured
fields: title, company, compensation, location, req id, and source. Sources are
pluggable adapters; v0 ships the platform-aware drop-in adapter — URLs
matching known boards (Greenhouse/Ashby/Lever/Workday) are fetched via the
board's public JSON API first, with HTML fetch as the fallback
(`docs/decisions/0003-api-first-extraction-in-v0.md`). Discovery
(watchlist polling) remains vNext.

### 2. Hard filters (gates)

Binary reject, not scored. A lead failing any gate is rejected and durably
recorded via a `rejected { gate, reason }` event. Gates:

- **remote-only** — reject confident non-remote positions only (explicit
  hybrid/on-site/in-office signals count as confident even when "remote"
  is mentioned somewhere). Unknown/unclear remote status passes to review
  and is flagged there — a false rejection is an invisible loss, a false
  pass costs one glance at a review card. (Confirmed 2026-08-26; pairs with
  the `remote` scoring dimension below that bubbles positive signals.)
- **compensation floor** — reject below the configured floor.
- **blacklist** — reject blacklisted companies (e.g., Salesforce).
- **ideological red lines** — the _mechanism_ must exist in v0 (a filter list),
  but the _content_ is deferred to a later LLM-based scorer (Remi).

### 3. Scoring

Each surviving lead gets per-dimension scores in 0–100, each carrying a
`confidence` field (default `1.0`; only meaningful once LLM scorers arrive).
Dimensions:

- **level** — a mix of signals: title (no standardized titles exist) and quoted
  "years of experience" in the posting.
- **skills** — keyword overlap between the JD and resume skills, plus a
  configurable alias table (e.g., `K8s` → `Kubernetes`).
- **compensation** — linear interpolation from floor to ceiling; at/above
  ceiling = 100. Unknown/missing comp passes the floor gate; the comp
  dimension drops out of the composite with weight renormalization, and the
  breakdown notes it.
- **remote** — positive remote signals bubble to the top: confident remote
  = 100, unknown = 50. (Confident non-remote never reaches scoring — the
  gate rejects it. Added 2026-08-26.)

Composite = weighted sum (`Σ(wᵢ·scoreᵢ) / Σwᵢ`), default equal weights,
configurable. The breakdown must be human-readable (e.g.,
`75 = 0.3·level(80) + 0.3·skills(90) + 0.4·comp(60)`) so I can debug _why_ a
lead ranked where it did. Emit a `scored` event.

### 4. Review queue

Ranked by composite score. Interactive CLI. The queue shows a deferral count
per lead (number of times marked defer). I mark each lead with one of:

| Mark                    | Meaning                                                                                                     |
| ----------------------- | ----------------------------------------------------------------------------------------------------------- |
| **apply-automatically** | This mark _is_ the approval (no second confirmation). Tool prepares the full package and opens the posting. |
| **apply-manual**        | I take personal action (internal contact, custom cover letter). Tool facilitates.                           |
| **defer**               | Stays in the queue; reappears next review.                                                                  |
| **ignore**              | Durable; never re-matches on future runs.                                                                   |

Emit a `reviewed` event. Marks are latest-wins projections over
`reviewed` events (re-marking a lead emits a new event; the projection
takes the latest).

### 5. Apply package

- **apply-automatically** — attach the generic cover letter (config path) + a
  generated "answers cheat sheet" (common ATS questions → resume-derived
  answers, rendered and displayed alongside; nothing is auto-filled in v0) +
  the resume PDF; emit `apply_queued`; open the posting URL.
  In v0 this degrades to manual submit; Playwright autofill is vNext.
- **apply-manual** — provide the JD + resume context + the same cheat sheet.

### 6. Event log

Append-only JSONL is the **source of truth**. An in-memory projection is
rebuilt on startup for the queue. SQLite projection is deferred.

## What It Should NOT Do

- Never auto-submit without explicit per-job approval. The "apply-automatically"
  mark _is_ that approval; the final click in the browser is expected and fine.
- Never fabricate or embellish experience or skills.
- Never contact people (recruiters, insiders) automatically.
- Be a good citizen: respect ToS and rate limits (e.g., 200–500ms delay between
  requests), obey control headers and redirects.
- Never send resume data to a service I haven't approved (moot in v0 — no
  LLM/API calls).
- Never write a cover letter from scratch; use the human-written corpus as the
  style guide (future constraint — v0 uses a generic letter).
- Never match blacklisted companies.

## Additional Context

- `~/Source/resume/resume.json` — machine-readable resume (JSON Resume schema).
  Source of truth for skills, experience, and the answers cheat sheet.
- `~/Source/gwl-job-tools/` — existing Rust crate (skeleton) to extend. Has a
  `Lead` model, event-sourcing intent, and JSONL storage intent. Its
  `lead open/list/close` subcommands are placeholders to be superseded; the
  style (clap + subcommands + miette + tracing) carries forward.
- `~/Documents/Job Hunt/events/events.csv` + `jds/` — the manual "alpha" of
  these features (example JDs and event types).
- `~/Documents/Job Hunt/cover/` — corpus of human-written cover letters
  (reference material now; style guide for future generation).

## Implementation Requirements

- **Language: Rust.** v0 is fully deterministic — no LLM, no API keys, no
  non-determinism. (LLM features are vNext.)
- **Extend `gwl-job-tools` in place**; redesign/augment the model for richer
  data (JD text, scores, state, actions, timestamps).
- **Event-sourcing**: append-only JSONL event log is the source of truth;
  in-memory projection for reads.
- **Config**: TOML file. Contents: compensation floor + ceiling, remote-only
  flag, blacklist, alias table, scoring weights, cover-letter path,
  target-companies list (empty for now).
- **Pluggable extension points** (design for, do not build): additional scoring
  types/sources (LLM scorers, embeddings), source adapters
  (Greenhouse/Ashby/Lever), cover-letter generation.
- **Delivery workflow**: deliver in small increments, each as a PR. PRs are the
  review checkpoint; the agent may continue ahead via stacked PRs, capped at 2.
  Before opening a new stacked PR, check for review feedback (Copilot, Remi,
  human) on existing PRs and incorporate it. Remi watches PRs and reviews.
  The first increment is a design PR: propose the event schema (event types +
  payloads) and the command surface, for review before building the pipeline.
  The design PR must answer: (a) lead identity — what uniquely identifies a
  lead (URL? req id? title+company hash?) and re-ingest semantics for
  updated/reposted leads; (b) the full lifecycle in the event schema,
  including outcome events (e.g., `applied`, `screened`) even if v0
  emits only a subset; (c) the alpha's fate — fresh start vs. one-time CSV
  import of `events.csv`; (d) the review queue's interaction model — a prompt
  loop is plenty for v0 (a ratatui TUI is a vNext-sized dependency).

  Increment 0 answered these in
  `docs/design/0001-event-schema-and-command-surface.md`. Two answers
  supersede this text: the alpha's fate is **fresh start** (no CSV import —
  `docs/decisions/0001-fresh-start-over-alpha-csv-import.md`), and the
  event/mark/command vocabulary was renamed (`no-action` → `defer`,
  `lead_`-prefixed event types drop the prefix, the queue command is `list`)
  — this document has been updated to match
  (`docs/decisions/0002-v0-naming.md`).

## Key Decisions (already made — do not re-litigate)

- Rust for the v0 core; Playwright (Python) shell-out is vNext.
- v0 is deterministic; no LLM/API calls.
- Event-sourcing: JSONL append-only log = source of truth; in-memory projection;
  SQLite deferred.
- Extend `gwl-job-tools` in place; redesign the model.
- Scoring = hard filters (gates) + weighted-sum composite; 0–100 with a
  confidence field.
- Unknown/missing comp passes the floor gate; the comp dimension drops out of
  the composite with weight renormalization.
- "apply-automatically" mark = the approval; no second confirmation; the final
  browser click is expected.
- Cover letters: generic letter for auto-applications; manual for others; no
  generation in v0.
- No age/deadline weighting (dropped as low-value complexity).
- Config = TOML.
- Delivery = incremental PRs, stacked cap 2, check review feedback before a new
  stacked PR.

## Acceptance Criteria (v0 definition of done)

v0 is done when I can:

1. Drop in a JD (URL or file) and get structured metadata.
2. Have it pass hard filters (remote, comp floor, blacklist) and produce a
   composite score with a human-readable breakdown.
3. See a ranked review queue.
4. Mark each lead apply-automatically / apply-manual / defer / ignore,
   durably.
5. For apply-automatically: get the full package (generic letter + cheat sheet +
   resume PDF) and the posting URL opened.
6. For apply-manual: get the JD + resume context + cheat sheet.
7. Every action produces an event in the event-sourcing log.

Browser auto-submit (Playwright) is explicitly out of v0.

## vNext Backlog (in rough order)

1. **Playwright autofill + open browser** (shell out to Python) — top of
   backlog. Target: correct autofill, I click the final Apply.
2. Greenhouse/Ashby/Lever source adapters ("finding") + configurable company
   watchlist.
3. Ideological alignment via Remi (LLM scorer).
4. Semantic skills matching (embeddings).
5. Cover-letter generation (corpus as style guide).
6. SQLite projection if querying gets slow.
7. Bonus/malus attribute scores.
