//! In-memory telemetry for the whole test binary (the tracing subscriber and the global
//! providers are process-wide): every test filters by its own trace id or attributes.

use std::sync::OnceLock;

use opentelemetry::trace::TracerProvider as _;
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_sdk::{
    logs::{InMemoryLogExporter, SdkLoggerProvider},
    metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider},
    propagation::TraceContextPropagator,
    trace::{InMemorySpanExporter, SdkTracerProvider},
};
use tracing_subscriber::{EnvFilter, Layer, layer::SubscriberExt};

pub struct TestTelemetry {
    pub spans: InMemorySpanExporter,
    pub metrics: InMemoryMetricExporter,
    pub meter_provider: SdkMeterProvider,
    pub logs: InMemoryLogExporter,
}

/// Installs the in-memory telemetry once; `common::state()` calls it, so it is in place
/// before any instrument is created.
pub fn telemetry() -> &'static TestTelemetry {
    static TELEMETRY: OnceLock<TestTelemetry> = OnceLock::new();
    TELEMETRY.get_or_init(|| {
        let spans = InMemorySpanExporter::default();
        let tracer_provider = SdkTracerProvider::builder()
            .with_simple_exporter(spans.clone())
            .build();
        let metrics = InMemoryMetricExporter::default();
        let meter_provider = SdkMeterProvider::builder()
            .with_reader(PeriodicReader::builder(metrics.clone()).build())
            .build();
        let logs = InMemoryLogExporter::default();
        let logger_provider = SdkLoggerProvider::builder()
            .with_simple_exporter(logs.clone())
            .build();
        opentelemetry::global::set_text_map_propagator(TraceContextPropagator::new());
        opentelemetry::global::set_tracer_provider(tracer_provider.clone());
        opentelemetry::global::set_meter_provider(meter_provider.clone());
        let subscriber = tracing_subscriber::registry()
            .with(tracing_opentelemetry::layer().with_tracer(tracer_provider.tracer("test")))
            .with(
                OpenTelemetryTracingBridge::new(&logger_provider)
                    .with_filter(EnvFilter::new("irisqo=debug")),
            );
        tracing::subscriber::set_global_default(subscriber).expect("global subscriber");
        // keep the logger provider alive for the whole run
        std::mem::forget(logger_provider);
        TestTelemetry {
            spans,
            metrics,
            meter_provider,
            logs,
        }
    })
}
