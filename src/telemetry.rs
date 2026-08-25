use clap::ValueEnum;
use miette::Result;
#[cfg(any(test, feature = "telemetry"))]
use tracing_subscriber::EnvFilter;
use tracing_subscriber::{registry, util::SubscriberInitExt};
#[cfg(feature = "telemetry")]
use {
    miette::IntoDiagnostic,
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
    tracing_subscriber::{fmt, layer::SubscriberExt},
};

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

pub fn init_telemetry(target: TelemetryStatus, name: &str) -> Result<TelemetryGuard> {
    match target {
        TelemetryStatus::Off => {
            let _ = name;
            registry().init();
            Ok(TelemetryGuard::NoProvider)
        }
        #[cfg(feature = "telemetry")]
        TelemetryStatus::On => {
            // Install a temporary subscriber so debug! calls in
            // build_otlp_provider are captured before the real subscriber is up.
            let _temp_guard = tracing::subscriber::set_default(
                registry()
                    .with(env_filter())
                    .with(fmt::layer().with_target(false).with_writer(std::io::stderr)),
            );

            let provider = build_otlp_provider(name)?;
            global::set_tracer_provider(provider.clone());

            let tracer = provider.tracer(name.to_owned());
            let telemetry_layer: OpenTelemetryLayer<_, SdkTracer> = OpenTelemetryLayer::new(tracer);

            registry()
                .with(env_filter())
                .with(fmt::layer().with_target(false).with_writer(std::io::stderr))
                .with(telemetry_layer)
                .init();

            Ok(TelemetryGuard::Otlp(provider))
        }
    }
}

#[cfg(any(test, feature = "telemetry"))]
fn env_filter() -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"))
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
        // The function always returns an EnvFilter (falls back to "info" if
        // RUST_LOG is unset). We just verify it doesn't panic.
        let _filter = env_filter();
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
