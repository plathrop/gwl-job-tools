# 0001: Fresh start over alpha CSV import

Status: accepted 2026-08-25 (design review of PR #3, Increment 0 /
GWLJ-rk2cnb)

## Context

The manual "alpha" of the job-search tool tracked 11 real outcomes in
`~/Documents/Job Hunt/events/events.csv` (8 `Applied`, 3 `Screened`), with
company, role, req id, URL, source, contact, and notes. The spec left the
alpha's fate as an Increment 0 question: fresh start vs. one-time CSV
import. The initial design-doc draft proposed a one-time
`gwl-jobs import-alpha` command, arguing the history is the only source of
dedupe-against-past-applications: without it, a reposted req for an
already-applied job (e.g. NVIDIA JR2018233) could re-enter the review queue
as if new.

## Options

- **One-time import** — a dedicated increment converting each CSV row into
  an `ingested` event plus one outcome event. Preserves history and re-apply
  protection.
- **Fresh start** — no import. Historical rows can be retro-recorded
  manually with `gwl-jobs outcome` (`--at` carries the original date) if a
  familiar posting resurfaces.

## Decision

Fresh start. No import command will be built.

## Rationale

- Eleven rows do not justify import machinery. Correct import idempotency
  requires row-level fingerprinting — dedupe keys identify the aggregate,
  not the imported row, so a re-run could not distinguish "lead already
  known" from "this row's outcome already appended" (flagged in review).
  That machinery, plus the mapping and a command used exactly once, is
  disproportionate.
- The only functional loss is re-apply protection for those 11 postings.
  The remedy is manual and cheap: the outcome event set subsumes the alpha's
  Applied/Screened vocabulary, and `gwl-jobs outcome` retro-records any row
  in seconds.
- **Accepted risk, explicitly:** a reposted req for an already-applied job
  may re-enter the review queue as if new.

## Consequences

- Design doc 0001 §6 answers question (c) "fresh start"; `import-alpha` is
  removed from the command surface; the spec is annotated accordingly.
- The alpha's `jds/` archive and cover-letter corpus remain on disk as
  reference material; nothing in the tool reads them.
- If an import is ever revived (a larger history appears, another source
  needs backfilling), the row-fingerprint idempotency design from the PR #3
  review must come with it — dedupe keys alone are insufficient.
