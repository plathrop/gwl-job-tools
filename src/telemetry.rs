use std::{fs::OpenOptions, path::Path, sync::Mutex};

use clap::ValueEnum;
use miette::{Context, IntoDiagnostic, Result};
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, registry, util::SubscriberInitExt};
#[cfg(feature = "telemetry")]
use {
    opentelemetry::{KeyValue, global, trace::TracerProvider as _},
    opentelemetry_otlp::{SpanExporter, WithExportConfig},
    opentelemetry_sdk::{
        Resource,
        trace::{SdkTracer, SdkTracerProvider},
    },
    opentelemetry_semantic_conventions::resource as semconv_resource,
    std::time::Duration,
    tracing::debug,
    tracing_opentelemetry::OpenTelemetryLayer,
};

use crate::config::LogLevel;

#[derive(Copy, Clone, Debug, ValueEnum)]
pub enum TelemetryStatus {
    #[value(name = "off", alias = "false")]
    Off,
    #[cfg(feature = "telemetry")]
    #[value(name = "on", alias = "true")]
    On,
}

pub enum TelemetryGuard {
    NoProvider,
    #[cfg(feature = "telemetry")]
    Otlp(SdkTracerProvider),
}

impl TelemetryGuard {
    pub fn shutdown(self) -> Result<()> {
        match self {
            #[cfg(feature = "telemetry")]
            Self::Otlp(provider) => provider.shutdown().into_diagnostic(),
            Self::NoProvider => Ok(()),
        }
    }
}

pub fn init_telemetry(
    target: TelemetryStatus,
    name: &str,
    log_level: Option<LogLevel>,
    log_path: &Path,
) -> Result<TelemetryGuard> {
    // Open the log file (append) before building the subscriber. A
    // configured path that can't be opened is a config error, not a silent
    // degradation (decision 0005).
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)
            .into_diagnostic()
            .wrap_err_with(|| format!("creating log dir {}", parent.display()))?;
    }
    let log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .into_diagnostic()
        .wrap_err_with(|| format!("opening log file {}", log_path.display()))?;

    let fmt_layer = fmt::layer()
        .with_target(false)
        .with_ansi(false)
        .with_writer(Mutex::new(log_file));

    match target {
        TelemetryStatus::Off => {
            let _ = name;
            registry()
                .with(env_filter(log_level))
                .with(fmt_layer)
                .init();
            Ok(TelemetryGuard::NoProvider)
        }
        #[cfg(feature = "telemetry")]
        TelemetryStatus::On => {
            // Install a temporary subscriber so debug! calls in
            // build_otlp_provider are captured before the real subscriber is
            // up. It writes to stderr: a transient debugging aid active only
            // during provider construction, and only emitting at debug level
            // (the default error level filters it out).
            let _temp_guard = tracing::subscriber::set_default(
                registry()
                    .with(env_filter(log_level))
                    .with(fmt::layer().with_target(false).with_writer(std::io::stderr)),
            );

            let provider = build_otlp_provider(name)?;
            global::set_tracer_provider(provider.clone());

            let tracer = provider.tracer(name.to_owned());
            let telemetry_layer: OpenTelemetryLayer<_, SdkTracer> = OpenTelemetryLayer::new(tracer);

            registry()
                .with(env_filter(log_level))
                .with(fmt_layer)
                .with(telemetry_layer)
                .init();

            Ok(TelemetryGuard::Otlp(provider))
        }
    }
}

/// Resolve the `tracing` filter: an explicit level (CLI or config) wins;
/// otherwise honor `RUST_LOG`, falling back to `error` (decision 0005).
fn env_filter(log_level: Option<LogLevel>) -> EnvFilter {
    match log_level {
        Some(level) => EnvFilter::new(level.as_str()),
        None => EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("error")),
    }
}

#[cfg(feature = "telemetry")]
fn build_otlp_provider(name: &str) -> Result<SdkTracerProvider> {
    // Per-signal vars take precedence over the base vars, per the OTel spec.
    let endpoint = std::env::var("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT")
        .ok()
        .or_else(|| std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").ok());
    debug!("using telemetry endpoint: {endpoint:?}");
    let headers = std::env::var("OTEL_EXPORTER_OTLP_TRACES_HEADERS")
        .ok()
        .or_else(|| std::env::var("OTEL_EXPORTER_OTLP_HEADERS").ok());
    debug!(
        "telemetry endpoint headers: {:?}",
        headers.as_deref().map(redact_header_values)
    );

    let version = env!("CARGO_PKG_VERSION");
    let resource = Resource::builder()
        .with_attributes([
            KeyValue::new(semconv_resource::SERVICE_NAME, name.to_owned()),
            KeyValue::new(semconv_resource::SERVICE_VERSION, version),
        ])
        .build();

    let exporter = SpanExporter::builder()
        .with_http()
        // Keep from hanging for a long time when an endpoint is
        // firewalled or otherwise blackholed.
        .with_timeout(Duration::from_millis(750))
        .build()
        .into_diagnostic()?;

    // Note: It is important that we use `with_simple_exporter` here
    // because as a CLI tool, we want spans transmitted immediately
    // rather than being batched. Otherwise the CLI either hangs at
    // exit or drops spans.
    let provider = SdkTracerProvider::builder()
        .with_simple_exporter(exporter)
        .with_resource(resource)
        .build();

    Ok(provider)
}

/// Redact the values in an OTEL headers env var (`k1=v1,k2=v2`), keeping the
/// key names, so API keys don't end up in logs. Values may themselves contain
/// `=` (e.g. base64 padding), so split on the first `=` only.
#[cfg(feature = "telemetry")]
fn redact_header_values(headers: &str) -> String {
    headers
        .split(',')
        .map(|pair| {
            let key = pair.split_once('=').map_or(pair.trim(), |(k, _)| k.trim());
            format!("{key}=<redacted>")
        })
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── TelemetryGuard ─────────────────────────────────────────────

    #[test]
    fn no_provider_shutdown_does_not_panic() {
        let guard = TelemetryGuard::NoProvider;
        let _ = guard.shutdown();
    }

    // ── env_filter ─────────────────────────────────────────────────

    #[test]
    fn env_filter_returns_a_filter() {
        // The function always returns an EnvFilter (falls back to "error" if
        // RUST_LOG is unset). We just verify it doesn't panic.
        let _filter = env_filter(None);
    }

    #[test]
    fn env_filter_explicit_level_maps_to_level_filter() {
        use tracing_subscriber::filter::LevelFilter;
        for (level, expected) in [
            (LogLevel::Error, LevelFilter::ERROR),
            (LogLevel::Warn, LevelFilter::WARN),
            (LogLevel::Info, LevelFilter::INFO),
            (LogLevel::Debug, LevelFilter::DEBUG),
            (LogLevel::Trace, LevelFilter::TRACE),
        ] {
            assert_eq!(env_filter(Some(level)).max_level_hint(), Some(expected));
        }
    }

    // ── TelemetryStatus (via clap) ────────────────────────────────

    // TelemetryStatus is a clap ValueEnum. It is tested implicitly
    // through the CLI parsing tests in cli.rs (parse_telemetry_*).
    // Adding a separate serde test is not applicable since the type
    // does not implement serde::Deserialize.

    // ── redact_header_values ────────────────────────────────────

    #[cfg(feature = "telemetry")]
    #[test]
    fn redacts_values_keeps_keys() {
        let redacted = redact_header_values("x-honeycomb-team=secret123,x-honeycomb-dataset=gwl");
        assert_eq!(
            redacted,
            "x-honeycomb-team=<redacted>, x-honeycomb-dataset=<redacted>"
        );
        assert!(!redacted.contains("secret123"));
        assert!(!redacted.contains("gwl"));
    }

    #[cfg(feature = "telemetry")]
    #[test]
    fn redacts_values_containing_equals() {
        // Values may contain '=' (e.g. base64 padding); split on first '=' only.
        let redacted = redact_header_values("Authorization=Basic abc==");
        assert_eq!(redacted, "Authorization=<redacted>");
        assert!(!redacted.contains("abc"));
    }

    #[cfg(feature = "telemetry")]
    #[test]
    fn redacts_whitespace_and_keyless_pairs() {
        assert_eq!(
            redact_header_values(" key = value , stray"),
            "key=<redacted>, stray=<redacted>"
        );
    }
}
