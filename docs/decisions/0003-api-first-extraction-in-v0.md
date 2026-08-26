# 0003: API-first extraction via board JSON APIs in v0

Status: accepted 2026-08-25 (design discussion on GWLJ-g8gbo3, confirmed in
PR #4 review)

## Context

The spec scopes Increment 1's ingest to a "drop-in adapter" with
Greenhouse/Ashby/Lever adapters deferred to vNext, where "adapters" meant
**discovery**: polling configurable watchlists for new postings
(GWLJ-7wlp89). During Increment 1 planning the question arose whether the
boards' public JSON APIs are also useful for **extraction** — given a URL
already in hand, fetch structured JSON instead of scraping HTML.

The posting URL already contains everything the API call needs; the
well-known URL patterns map directly:

- `job-boards.greenhouse.io/{board}/jobs/{id}` → `boards-api.greenhouse.io/v1/boards/{board}/jobs/{id}`
- `jobs.ashbyhq.com/{board}/{jobId}` → `api.ashbyhq.com/posting-api/job-board/{board}/{jobId}`
- `jobs.lever.co/{company}/{id}` → `api.lever.co/v0/postings/{company}/{id}`
- `*.myworkdayjobs.com/.../job/...` → the semi-public Workday CXS JSON
  endpoint (`/wday/cxs/{tenant}/{site}/job/{path}`) — significant because
  the alpha history is heavily Workday, and Workday pages are JS-rendered
  shells that HTML extraction handles worst.

## Options

- **Keep Increment 1 strictly HTML drop-in**; add API-backed extraction
  later alongside the discovery adapters.
- **Fold API-backed extraction into the drop-in adapter now** (chosen).

## Decision

The v0 drop-in adapter is **platform-aware**: pattern-match the dropped URL
against known boards and fetch the board's public JSON API **first**,
falling back to HTML fetch + main-text extraction for unknown sites or
unrecognized API responses. `source` records the platform
(`greenhouse`/`ashby`/`lever`/`workday`) or `drop-in`. Discovery (watchlist
polling) remains vNext.

## Rationale

- **Anti-scraping pressure**: the boards are actively pursuing
  anti-scraping measures; their public APIs are the sanctioned, stable path.
  API-first also means fewer requests against protected frontends.
- **Extraction quality**: structured title/company/location/req id and a
  clean description body with no page chrome to strip.
- **Determinism and good-citizenship are unchanged**: no LLM, same
  politeness/rate-limit rules.
- Comp coverage is spotty on the APIs just as it is on the sites, so
  best-effort text extraction runs over the API body regardless.

## Consequences

- The spec's "v0 ships only the drop-in adapter" line is annotated to
  reflect this; design doc 0001 §3 documents `source` values.
- PDF/print-to-PDF (GWLJ-mxyp63) remains the escape hatch for JS-rendered
  or anti-scraping sites with no usable API.
- Platform response shapes are undocumented and drift; parsing is defensive
  (`Value` probing), and an unrecognized response falls back to HTML rather
  than failing the ingest.
