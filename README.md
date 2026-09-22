# gwl-job-tools

An event-sourced job-search triage CLI. Drop in a job posting and `gwl-jobs`
deterministically ingests it, runs it through hard reject gates and a weighted
score, and puts it in a review queue — so you spend your attention on the
high-value decision (which jobs to pursue), not the low-judgment work of
sifting and scoring.

v0 is fully deterministic: no LLM, no API keys, no network calls beyond
fetching the posting you point it at.

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

## Commands

| Command                                                   | Purpose                                                                                                              |
| --------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------- |
| `gwl-jobs ingest <url>` or `--file <path>`                | Fetch, extract, dedupe, gate, score a posting.                                                                       |
| `gwl-jobs list [--all]`                                   | Print the active pipeline (non-terminal, non-ignored), ranked.                                                       |
| `gwl-jobs review`                                         | Interactive review queue (§5 of the design doc).                                                                     |
| `gwl-jobs mark <lead> <mark> [--note]`                    | Non-interactive mark; `apply-automatically` runs the full prepare → open flow.                                       |
| `gwl-jobs edit <lead> [flags]`                            | Correct fields extraction missed (title, company, comp, remote, …).                                                  |
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
compensation_ceiling = 400000
remote_only = false # reject confident non-remote postings
reject_location_only = false # treat location-only postings as non-remote
blacklist = ["salesforce"] # never match these companies
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
```

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
