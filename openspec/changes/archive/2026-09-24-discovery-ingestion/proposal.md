## Why

v0 ingests one posting at a time, by hand (URL or file drop). The
energy-economics goal — convert scarce daily energy into quality
applications — stalls when the only way to fill the review queue is manual
pasting. The tool needs to *discover* new leads itself, so it becomes the
single interface to the search. This change builds the machinery for batch
ingestion from multiple feed sources (the "discovery layer"), with a free
feed wired up first to prove the interface.

## What Changes

- Add a `discover` subcommand that fetches job postings from one or more
  configured feed sources and runs each through the existing ingest
  pipeline (fetch → extract → gate → score → dedupe → persist).
- Introduce a pluggable `DiscoverySource` trait so feed sources — free
  curated feeds now, paid commercial APIs later — are adapters behind one
  interface, not bespoke code paths.
- Make URL ingestion redirect-aware: a discovered posting's (possibly proxy)
  URL is resolved to its canonical posting URL, and known ATS platforms are
  detected from the *resolved* URL — so a feed proxy URL gets the same
  API-first extraction and the same strong dedupe keys (`req:`/`url:`) as a
  directly-pasted posting URL.
- Record each discovered lead with `source` = the feed that found it and
  `adapter` = the extraction platform (ATS), preserving existing field
  semantics.
- Guarantee batch consistency: processing a batch of postings never mints
  duplicate leads for the same job, even within a single run.
- Emit a per-run summary (new / updated / suppressed / rejected) so the
  operator can see what a discovery run did.

**Observability** (behavior-bearing change): the operator must be able to
answer "did this run find good leads, and how were they handled?" — the
command emits a run summary plus a span per source fetch and per posting
ingest (feed, canonical URL, outcome), with errors carrying the offending
URL. Details in design.md.

## Capabilities

### New Capabilities

- `discovery`: batch ingestion of job postings from pluggable feed sources —
  posting discovery, canonical URL resolution, and feeding postings through
  the existing ingest/dedupe pipeline without duplicating leads.

### Modified Capabilities

None.

## Impact

- **Code**: new `discovery` module (trait + feed adapters + batch driver),
  a `discover` command, a `[sources.<name>]` config section,
  redirect-aware canonicalization in `ingest` (`HttpFetcher` final-URL
  capture + platform re-detection), and a projection-consistency fix in the
  batch driver.
- **First adapter**: Remotive (free, deterministic JSON). WWR and paid
  sources (TheirStack/Coresignal) follow behind the same trait.
- **Dependencies**: none new (reuses reqwest/tokio/serde).
- **No breaking changes** to the event schema or existing commands.
