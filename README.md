# gwl-job-tools

An event-sourced job-search triage CLI. Drop in a job posting and `gwl-jobs`
deterministically ingests it, runs it through hard reject gates and a weighted
score, and puts it in a review queue — so you spend your attention on the
high-value decision (which jobs to pursue), not the low-judgment work of
sifting and scoring.

## Install

```sh
git clone https://github.com/plathrop/gwl-job-tools
cd gwl-job-tools
cargo install --path .
```

Requires a Rust nightly toolchain (see `rust-toolchain.toml`).

## Quick start

```sh
# Drop in a posting (URL, or local HTML/text file)
gwl-jobs ingest https://boards.greenhouse.io/acme/jobs/1234
gwl-jobs ingest --file jd.html

# Step through the pending queue, highest score first
gwl-jobs review

# Non-interactive marks (scriptable)
gwl-jobs mark <lead> apply-automatically
gwl-jobs mark <lead> defer

# Record outcomes as the search progresses
gwl-jobs applied <lead> --method manual
gwl-jobs outcome <lead> rejected_by_employer
```

Leads are addressed by an unambiguous UUID prefix, e.g. `gwl-jobs show 0192f8a1`.

## Pipeline

Every posting moves through a fixed pipeline over an append-only,
event-sourced log:

1. **Ingest** — platform-aware extraction (Greenhouse / Ashby / Lever /
   Workday public JSON APIs first, HTML main-text fallback) of title,
   company, compensation, location, remote signal, req id, and source.
2. **Gate** — hard binary rejections: remote-only, compensation floor,
   company blacklist, and (mechanism-only in v0) ideological red lines.
   Unknown/missing compensation passes the floor gate.
3. **Score** — a deterministic weighted-sum composite (0–100) over level,
   skills, compensation, and remote, each with a confidence field and a
   human-readable breakdown. A dimension that can't be scored drops out with
   weight renormalization.
4. **Review** — an interactive queue ranked by score; mark each lead
   `apply-automatically`, `apply-manual`, `defer`, or `ignore`.
5. **Apply** — for `apply-automatically` leads, assemble a package (generic
   cover letter + ATS answer cheat sheet + resume) and open the posting. The
   final submit click is always yours.

`gwl-jobs discover` runs this pipeline in batch over configured feed
sources — free curated feeds (Remotive) or the paid TheirStack Jobs API —
resolving each posting to its canonical URL first.

## Commands

| Command                                                   | Purpose                                                                                                              |
| --------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------- |
| `gwl-jobs ingest <url>` or `--file <path>`                | Fetch, extract, dedupe, gate, score a posting.                                                                       |
| `gwl-jobs discover [--source <name>] [--dry-run] [--yes]` | Batch-ingest postings from enabled feed sources (same pipeline as `ingest`).                                         |
| `gwl-jobs list [--all]`                                   | Print the active pipeline (non-terminal, non-ignored, not gate-rejected), ranked.                                    |
| `gwl-jobs review`                                         | Interactive review queue (§5 of the design doc).                                                                     |
| `gwl-jobs mark <lead> <mark> [--note]`                    | Non-interactive mark; `apply-automatically` runs the full prepare → open flow.                                       |
| `gwl-jobs edit <lead> [flags]`                            | Correct fields extraction missed (title, company, comp, remote, …).                                                  |
| `gwl-jobs rescore <lead> \| --all [--dry-run]`            | Re-run gates + scoring with the current config over the stored snapshot (e.g. after a config fix); no refetch.       |
| `gwl-jobs package <lead>`                                 | Rebuild and reopen the apply package for an apply-automatically lead.                                                |
| `gwl-jobs show <lead> [--jd]`                             | Show a lead's projected state, or the raw posting text with `--jd`.                                                  |
| `gwl-jobs applied\|screened\|interviewed\|offered <lead>` | Record a non-terminal transition.                                                                                    |
| `gwl-jobs outcome <lead> <type>`                          | Record a terminal outcome (`accepted`, `rejected_by_employer`, `withdrawn`, `declined`, `unresponsive`, `archived`). |
| `gwl-jobs events [--lead <id>] [--type <t>]`              | Dump/filter the raw event log.                                                                                       |
| `gwl-jobs completion [shell]`                             | Shell completions on stdout (bash/zsh/fish).                                                                         |

Global flags: `--json` (machine-readable output), `--data-dir <path>`,
`--config <path>`, `--log-level`, `--telemetry on|off`, `--color`.

## Configuration

Config lives at `<config_dir>/config.toml` (per-OS, namespaced under
`st.ember/gwl`; see `directories::ProjectDirs`). A missing file means
defaults. All keys:

```toml
compensation_floor = 180000 # reject below, USD/year
compensation_ceiling = 400000 # leads at or above this score 100%
remote_only = false # reject confident non-remote postings
reject_location_only = false # treat location-only postings as non-remote
blacklist = ["initech"] # never match these companies
target_companies = []

[aliases] # skill synonyms
K8s = "Kubernetes"

[scoring_weights] # default: equal weights
level = 1.0
skills = 1.0
compensation = 1.0
remote = 1.0

resume_path = "~/resume.json" # JSON Resume (skills + cheat sheet)
cover_letter_path = "~/letter.pdf"
ideological_red_lines = [] # mechanism ships in v0; content is vNext
log_level = "error" # error | warn | info | debug | trace
log_file = "/path/to/gwl-jobs.log"
telemetry = "off" # opt-in OTLP traces to Honeycomb

[sources.remotive] # opt-in feed sources for `discover` (default disabled)
enabled = true

[sources.theirstack] # paid TheirStack Jobs API (1 credit per returned job)
enabled = true
api_key = "${THEIRSTACK_API_KEY}" # env-var reference, resolved config-wide
posted_at_max_age_days = 30 # recency window for the first backfill
strict_filtering = false # opt-in: push remote/comp gates server-side
max_credits_per_run = 750 # optional: cap one run's spend (records = credits)

[sources.theirstack.query] # optional: any documented TheirStack filter
job_seniority_or = ["senior", "staff"]
job_technology_slug_or = ["kubernetes", "terraform"]
job_country_code_or = ["US", "CA"]
```

Discovery sources are opt-in and fetched only when `enabled = true`. The
TheirStack source charges one API credit per returned job, so `discover`
records a per-source `discovered_at` watermark on each run and re-runs fetch
only newly-discovered jobs. A `--dry-run` that would spend credits prompts on
stderr (`--yes` skips the prompt; without `--yes` it refuses under `--json` or
when stdin isn't a terminal). The configured blacklist is additionally pushed server-side as a
credit-saving pre-filter, but the client-side blacklist gate remains the
authoritative backstop. A paid fetch that fails partway (e.g. credits
exhausted mid-pagination, a 402) keeps the records already paid for — they
are ingested and the watermark advances — while the source is still reported
as failed, with the reason, in the summary and the run event.

The `[sources.theirstack.query]` table spends credits only on postings you
chose: every entry is merged into the TheirStack search request, so you can
narrow the feed server-side (titles, technologies, seniority, countries,
salary, workplace type — any parameter TheirStack documents; build a search
in their app UI and copy the cURL). Keys the adapter itself owns (`limit`,
`page`, `cursor`, `posted_at_max_age_days`, `discovered_at_gte`,
`company_name_not`, `company_domain_not`) are rejected at load time;
`workplace_types_or` and
`min_salary_usd` are rejected only when `strict_filtering` is on (with it
off, they're yours — e.g. `workplace_types_or = ["remote", "hybrid"]`).
Everything a narrowed feed returns still passes through every client-side
gate and the scoring pipeline — filtering is a spend decision, not a gate.

Paid fetches are bounded: before each page the adapter checks the free
credit-balance endpoint and caps the page to what the balance can pay for,
so a small balance is a small fetch instead of a rejected one — and
`max_credits_per_run` bounds a single run's spend regardless of balance.
When either bound stops the run, already-paid records are ingested and the
stop is reported with its reason. The summary reports credits spent per
paid source (`credits spent: theirstack 750`).

## How it works

The **JSONL event log** (`<data_dir>/events.jsonl`) is the source of truth:
one event per line, append-only, never rewritten. Each command appends events
(`ingested`, `rejected`, `scored`, `reviewed`, `applied`, …); a read model is
rebuilt in memory by replaying the log at startup. There is no database —
replaying the log reproduces the entire state, and a torn trailing line (a
crash mid-write) is discarded and truncated rather than corrupting the log.

See
[`docs/design/0001-event-schema-and-command-surface.md`](docs/design/0001-event-schema-and-command-surface.md)
for the authoritative event schema and command surface, and
[`docs/archive/job-search-automation-v2.md`](docs/archive/job-search-automation-v2.md)
for the original specification.

## Guardrails

`gwl-jobs` never auto-submits an application without your explicit per-job
approval (the `apply-automatically` mark *is* that approval; the final
browser click is always yours). It never fabricates or embellishes your
experience, never contacts people automatically, and never matches
blacklisted companies.

## License

MIT
