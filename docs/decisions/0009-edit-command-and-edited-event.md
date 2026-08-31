# 0009: The `edit` command and the `edited` event

Status: accepted 2026-08-30

## Context

Extraction is best-effort: some boards (e.g. Wellfound) yield a title and
company but no comp, remote, or location — so a lead can score 61 with the
compensation dimension renormalized away and `remote(50)` (unknown) purely
because the extractor missed fields the user actually has. Until source
adapters improve, the only remedies were re-ingesting (pointless when the
source is the problem) or nothing.

Design discussion (Grey, 2026-08-30) settled a manual correction command:
`gwl-jobs edit <lead> …`.

## Decision

- **A new `edited` event, not a reuse of `updated`.** The payload mirrors
  `updated` (a full snapshot + `changed` list, `adapter: "user"`, optional
  `note` for provenance) so the aggregate and projection treat it as a
  member of the snapshot-event family — revision counting, score
  invalidation, and rejection clearing all come free. A distinct event
  type keeps the door open for field-level merge protection (below) that a
  reused `updated` could not distinguish.
- **No clobber-protection for now.** A subsequent re-ingest snapshot
  replaces the whole snapshot, including user-corrected fields. Accepted:
  re-ingests are rare at this volume (exactly one so far, and `edit`
  removes that use), and protection has subtle interactions with the
  projection's "mirror the event snapshot exactly" invariant.
- **Flags-only CLI for v0** — `--title`, `--company`, `--req-id`,
  `--location`, `--remote true|false|unknown`, `--comp` (parsed through the
  same `extract_comp` the ingest path uses) with `--comp-min`/`--comp-max`
  for exact bounds, `--url`, `--source`, and an explicit `--clear
field,…` (no empty-string sentinels). `$EDITOR`-based bulk editing is
  filed as a follow-up (GWLJ-hi6szo).
- **The dedupe key is immutable.** An edit never changes the stored key;
  identifiers recomputed from the corrected fields are indexed
  _additively_ (old forms stay), and an edit that would collide with a
  _different_ lead's identity is refused — that is a merge, not an edit.
- **Edits re-evaluate like re-ingests**: gates and scoring run on the
  corrected content, appending `edited` + (`rejected` | `scored`) in one
  batch. This is the point: fixing a gate failure or a missing comp
  immediately restores the lead to the pending queue.
- **Edits bypass durable-ignore suppression** — the command is explicit
  user action — but the mark stays latest-wins, so an ignored lead still
  does not re-enter the queue.

## Rationale

- The snapshot-event family design means `evolve`/projection changes are
  one match-arm extension, not new machinery.
- Tri-state `--remote true|false|unknown` matches the `Option<bool>` in
  `ExtractedFields` exactly; `unknown` and `--clear remote` agree.
- Reusing `extract_comp` keeps one comp-parsing brain; a form it cannot
  parse is a loud error, never a silent drop.
- `--source` on `edit` replaces the "re-ingest to set the source"
  workaround in practice.

## Consequences

- Design doc 0001 §3 gains the `edited` payload; §8 gains the `edit` row;
  §2 notes the clobber limitation.
- A later increment may add merge protection over user-corrected fields
  using the `edited` events' `changed` lists as the seam.
- `$EDITOR` bulk editing is future work (GWLJ-hi6szo), not scope creep
  here.
