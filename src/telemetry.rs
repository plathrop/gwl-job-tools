use std::time::Duration;

use clap::ValueEnum;
use miette::{IntoDiagnostic, Result};
use opentelemetry::{global, trace::TracerProvider as _, KeyValue};
use opentelemetry_otlp::{SpanExporter, WithExportConfig};
use opentelemetry_sdk::{
    trace::{SdkTracer, SdkTracerProvider},
    Resource,
};
use opentelemetry_semantic_conventions::resource;
use tracing_opentelemetry::OpenTelemetryLayer;
use tracing_subscriber::{fmt, layer::SubscriberExt, registry, util::SubscriberInitExt, EnvFilter};

#[derive(Copy, Clone, Debug, ValueEnum)]
pub enum TelemetryStatus {
    #[value(name = "off", alias = "false")]
    Off,
    #[value(name = "on", alias = "true")]
    On,
}

pub enum TelemetryGuard {
    NoProvider,
    Otlp(SdkTracerProvider),
}

impl TelemetryGuard {
    pub fn shutdown(self) {
        if let Self::Otlp(provider) = self {
            // Telemetry should never make the actual command fail.
            if let Err(err) = provider.shutdown() {
                eprintln!("warning: failed to shutdown telemetry provider: {err}");
            }
        }
    }
}

pub fn init_telemetry(target: TelemetryStatus, name: &str) -> Result<TelemetryGuard> {
    match target {
        TelemetryStatus::Off => {
            registry().init();
            Ok(TelemetryGuard::NoProvider)
        }
        TelemetryStatus::On => {
            let provider = build_otlp_provider(&name)?;
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

fn env_filter() -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"))
}

fn build_otlp_provider(name: &str) -> Result<SdkTracerProvider> {
    let version = env!("CARGO_PKG_VERSION");
    let resource = Resource::builder()
        .with_attributes([
            KeyValue::new(resource::SERVICE_NAME, name.to_owned()),
            KeyValue::new(resource::SERVICE_VERSION, version),
        ])
        .build();

    let exporter = SpanExporter::builder()
        .with_http()
        // Keep a CLI from hanging for a long time when an endpoint is firewalled
        // or otherwise blackholed.
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

#[cfg(test)]
mod tests {
    use super::*;

    // ── TelemetryGuard ─────────────────────────────────────────────

    #[test]
    fn no_provider_shutdown_does_not_panic() {
        let guard = TelemetryGuard::NoProvider;
        guard.shutdown();
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
}
