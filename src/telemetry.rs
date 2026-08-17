use miette::{IntoDiagnostic, Result};
use opentelemetry::trace::TracerProvider as _;
use opentelemetry::{global, KeyValue};
use opentelemetry_otlp::SpanExporter;
use opentelemetry_sdk::trace::{SdkTracer, SdkTracerProvider};
use opentelemetry_sdk::Resource;
use opentelemetry_semantic_conventions::resource;
use tracing_opentelemetry::OpenTelemetryLayer;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{fmt, registry, EnvFilter};

pub const APP_NAME: &str = "gwl-jobs";

pub fn init_telemetry() -> Result<SdkTracerProvider> {
    let version = env!("CARGO_PKG_VERSION");
    let resource = Resource::builder()
        .with_attributes([
            KeyValue::new(resource::SERVICE_NAME, APP_NAME),
            KeyValue::new(resource::SERVICE_VERSION, version),
        ])
        .build();

    let exporter = SpanExporter::builder()
        .with_http()
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

    global::set_tracer_provider(provider.clone());

    let tracer = provider.tracer(APP_NAME);

    let telemetry_layer: OpenTelemetryLayer<_, SdkTracer> = OpenTelemetryLayer::new(tracer);
    let fmt_layer = fmt::layer().with_target(false);
    let filter_layer = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    registry()
        .with(filter_layer)
        .with(fmt_layer)
        .with(telemetry_layer)
        .init();

    Ok(provider)
}
