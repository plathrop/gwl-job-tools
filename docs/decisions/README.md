# Decision Records

Numbered decision records: `NNNN-short-slug.md` (e.g.,
`0001-jsonl-event-log-over-sqlite.md`).

A decision record captures a point-in-time choice: context, options
considered, decision, and rationale. Records are append-only — if a decision
is reversed, add a new record that supersedes the old one; don't rewrite
history.

The spec (`docs/specs/job-search-automation-v2.md`) carries a Key Decisions
section that is settled; those do not need to be retro-recorded here unless
one is revisited.

For living descriptions of how the system works, use a design doc in
`docs/design/` instead.
