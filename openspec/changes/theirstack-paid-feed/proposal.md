# Proposal

## Why

The discovery layer has one free adapter (Remotive). Grey holds a paid
TheirStack subscription, and its Jobs API is shaped for exactly this search —
server-side filters on remote/comp/tech/title/seniority, plus the full
posting text in every record — so wiring it up turns discovery from a light
free feed into the high-quality, high-volume ingestion path the tool was
built for. The paid tier also demands credit budgeting (one API credit per
record returned), or every re-run re-spends money on jobs already seen.

## What Changes

- **Snapshot-capable discovery seam**: `DiscoverySource` yields
  already-extracted postings, not just URLs; each adapter decides whether it
  re-extracts (Remotive, proxy URLs) or maps records directly (TheirStack).
  The `Posting` hints struct dissolves — its client-side pre-filter role is
  obsolete now that pre-filtering is server-side. **BREAKING** (library API):
  `DiscoverySource::postings()` becomes `fetch()`, and `Posting` is removed.
- **TheirStack Jobs API adapter** behind `[sources.theirstack]`, keyed by the
  `THEIRSTACK_API_KEY` env var (`Authorization: Bearer`).
- **Blacklist pre-filter server-side (additive)**: the blacklist gate stays
  in the client-side pipeline — unchanged and authoritative. Sources that can
  exclude companies in their query additionally push the blacklist there, so
  blacklisted companies are never fetched — and, for a paid source, never
  charged — as a credit-saving optimization, not a replacement for the gate.
  Remote-only and compensation-floor gates stay client-side — they are
  permissive ("unknown passes"), and pushing them to strict server filters
  would false-reject unknown-workplace / unknown-salary jobs (the invisible
  loss the spec forbids). Skills is a score dimension, never a feed filter.
- **Opt-in strict filtering**: a per-source `strict_filtering` toggle
  (default off) lets a source push the permissive gates into its query; each
  adapter documents what its strict mode excludes. The default stays
  client-side until the null-rate data justifies going strict.
- **Trust-but-verify extraction**: TheirStack's structured fields are
  authoritative; our own text heuristics still run over the description, and
  any disagreement is emitted as spans/logs — never blocking.
- **Watermark cursor**: the max `discovered_at` fetched is stored on the
  discovery run event and passed back as `discovered_at_gte` next run, so
  re-runs fetch only newly-discovered jobs.
- **Credit observability**: `metadata.truncated_results` (credit exhaustion)
  surfaces in the run summary. Live-verified 2026-10-07: exhaustion also
  surfaces as a mid-pagination **402** whose body states the required/available
  credits — so the adapter salvages already-fetched records instead of
  discarding them, mines the 402 body for the unreturned count, and every
  source failure (whole or partial) carries its reason into the summary,
  the run event, and the error-level log (GWLJ-w9xhcg).

## Capabilities

### New Capabilities

None.

### Modified Capabilities

- `discovery`: sources may yield already-extracted postings; the TheirStack
  paid source; a blacklist server-side pre-filter (client gate unchanged);
  opt-in strict filtering; the watermark cursor; and credit observability.

## Impact

- **Code**: `discovery` module (trait change + `theirstack` adapter + query
  builder), `discover` command (query-shaping + cursor wiring), config
  (`[sources.theirstack]` + `strict_filtering` + credential env), event
  schema (additive watermark field on the `discovery` run payload).
- **Dependencies**: none new (reqwest/serde/serde_json).
- **No breaking changes** to the event schema or existing commands; the only
  break is the library-level `DiscoverySource`/`Posting` surface noted above,
  which is v0.1-internal and pre-1.0. Remotive behavior is unchanged (tests
  prove parity); `discover` with no TheirStack config is unaffected.
- **Observability** (decision 0011): per-source and per-posting spans gain
  TheirStack-specific fields; trust-but-verify mismatch spans/logs; and
  credit-exhaustion field in the summary. The strict-vs-client-side
  question is answered by data, not argument: source spans and the summary
  record the null-rate facts (unknown workplace type / salary, and the count
  strict mode would drop), and the review path records the mark alongside the
  lead's ingest-time status so Honeycomb can correlate what the operator
  actually does with unknown-status leads.
