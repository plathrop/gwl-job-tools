# 0004: Own resume.json validation (official crate not ready)

Status: accepted 2026-08-28 (planning for Increment 3 / GWLJ-1hp1r6)

## Context

Increment 3 (scoring) needs the resume's skills for the skills dimension;
Increment 5 (apply package) needs `basics`/`work` for the answers cheat
sheet. The spec points at `~/Source/resume/resume.json` (JSON Resume
schema). The official Rust implementation — `json-resume-serde`
(jsonresume.org monorepo, `packages/core-rust`) — was considered for
parsing/validation.

## Options

- **Use the official crate** (`json-resume-serde`) for typed parsing and
  `validate()`.
- **Own validation** — minimal, lenient structs for the fields we consume,
  plus light structural checks.

## Decision

Own validation. Do not add the official crate to the dependency chain.

## Rationale

- The crate is v0.1.0 and **not published to crates.io** ("Publishing to
  crates.io is TBD — depend via a Git or path dependency"), so it would be a
  git dependency.
- Its `Resume` struct declares several sections as required `Vec` fields
  without `#[serde(default)]` (`volunteer`, `education`, `awards`,
  `certificates`, `publications`, `languages`, `interests`). Grey's actual
  `resume.json` omits those sections, so `serde_json::from_str::<Resume>`
  fails on the real file.
- v0 consumes only `skills` (scoring) and `basics`/`work` (cheat sheet).
  Minimal lenient structs for those fields are simpler and sufficient for a
  deterministic v0.

## Consequences

- `resume_path` becomes a config value (path to a JSON Resume file).
- Parsing is lenient (unknown fields ignored, missing sections defaulted);
  validation is light structural checks of the fields we consume.
- When `resume_path` is set but the file is missing or unparseable, the
  command fails loudly. When unset, the skills dimension degrades and a WARN
  is logged (see decision 0005 for the logging story).
- No git dependency on `json-resume-serde`. Revisit if the crate is
  published and its strictness is relaxed.
