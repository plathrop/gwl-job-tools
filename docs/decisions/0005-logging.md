# 0005: Logging — file sink, configurable level, default error

Status: accepted 2026-08-28 (planning for the logging increment)

## Context

`gwl-jobs` is a user-facing CLI. `tracing` log lines must not pollute
stdout (command output) or stderr (miette error reports). The pre-existing
setup installed a subscriber with no layers by default (logs dropped) and,
with `--telemetry on`, a `fmt` layer writing to stderr at `info`.

## Decision

- **Sink**: a `tracing` `fmt` layer writes to a log file (append mode, no
  rotation). Nothing on stdout/stderr. miette errors still go to stderr —
  that is error _reporting_, not logging, and is unchanged.
- **Level**: configurable. Config key `log_level` (default `error`); CLI
  `--log-level` overrides config.
- **Precedence**: CLI arg > config > `RUST_LOG` > default `error`.
- **File location**: config key `log_file` (optional); default
  `<data_dir>/gwl-jobs.log` via `directories`.
- **Telemetry**: `--telemetry on` keeps the OTLP layer; the `fmt` layer
  moves from stderr to the file.

## Rationale

- A user-facing utility should not interleave log noise with its output or
  its error reports (the project's energy-economics goal: low-friction
  reading).
- `error` default keeps the file quiet in normal operation; `RUST_LOG`
  remains the familiar debugging escape hatch.
- Rotation is unnecessary at CLI volume (a handful of lines per run).

## Consequences

- `Config::load` is hoisted to `main` (before subscriber init) so the log
  level can be resolved from config; commands receive `&Config` instead of
  loading it themselves.
- `init_telemetry` is reworked to take the resolved level and log path; the
  `fmt` layer writes to the file via `Mutex<File>` (no new dependency).
- The resume.json "no path set → WARN" behavior (decision 0004) depends on
  this sink/level story.
