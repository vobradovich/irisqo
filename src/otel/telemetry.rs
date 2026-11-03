use opentelemetry::trace::TracerProvider as _;
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_sdk::{
    Resource,
    logs::SdkLoggerProvider,
    metrics::{PeriodicReader, SdkMeterProvider},
    propagation::TraceContextPropagator,
    trace::SdkTracerProvider,
};
use tracing::Level;
use tracing_subscriber::{
    EnvFilter, Layer, filter::Targets, layer::SubscriberExt, util::SubscriberInitExt,
};

use super::TRACING_TARGET;

/// Logs of these crates are never exported as OTel logs: the exporter itself uses them,
/// exporting them would feed back into the exporter.
const LOG_EXPORT_DENY: &str = "hyper=off,h2=off,reqwest=off,tonic=off,opentelemetry=off";

/// Traces, metrics and logs; [`Telemetry::shutdown`] flushes them.
#[derive(Debug)]
pub struct Telemetry {
    tracer_provider: SdkTracerProvider,
    meter_provider: SdkMeterProvider,
    logger_provider: SdkLoggerProvider,
}

/// Exporter of a signal from the standard env vars: `OTEL_{TRACES,METRICS,LOGS}_EXPORTER`
/// = `otlp` | `console` | `none`; without it `otlp` when an OTLP endpoint is set
/// (`OTEL_EXPORTER_OTLP_ENDPOINT` or `OTEL_EXPORTER_OTLP_{signal}_ENDPOINT`), `none` otherwise.
fn exporter_kind(signal: &str) -> String {
    std::env::var(format!("OTEL_{signal}_EXPORTER")).unwrap_or_else(|_| {
        let has_endpoint = std::env::var_os("OTEL_EXPORTER_OTLP_ENDPOINT").is_some()
            || std::env::var_os(format!("OTEL_EXPORTER_OTLP_{signal}_ENDPOINT")).is_some();
        if has_endpoint { "otlp" } else { "none" }.into()
    })
}

fn report<T, E: std::fmt::Display>(signal: &str, res: Result<T, E>) -> Option<T> {
    res.inspect_err(|err| eprintln!("OTLP {signal} exporter error, not exported: {err}"))
        .ok()
}

impl Telemetry {
    /// Sets the W3C propagator, the global tracer and meter providers and the global
    /// tracing subscriber: logs to stdout (filtered by `RUST_LOG`) and to OTel,
    /// `otel::tracing` spans to OTel.
    ///
    /// OTLP is HTTP/protobuf, configured by the `OTEL_EXPORTER_OTLP_*` vars;
    /// `OTEL_SERVICE_NAME` overrides the service name, `OTEL_METRIC_EXPORT_INTERVAL` the
    /// metrics export interval (ms).
    pub fn init() -> Telemetry {
        let service_name =
            std::env::var("OTEL_SERVICE_NAME").unwrap_or_else(|_| env!("CARGO_PKG_NAME").into());
        let resource = Resource::builder().with_service_name(service_name).build();

        let mut traces = SdkTracerProvider::builder().with_resource(resource.clone());
        match exporter_kind("TRACES").as_str() {
            "otlp" => {
                let exporter = opentelemetry_otlp::SpanExporter::builder()
                    .with_http()
                    .build();
                if let Some(exporter) = report("traces", exporter) {
                    traces = traces.with_batch_exporter(exporter);
                }
            }
            "console" => {
                traces = traces.with_simple_exporter(opentelemetry_stdout::SpanExporter::default());
            }
            _ => {}
        }

        let mut metrics = SdkMeterProvider::builder().with_resource(resource.clone());
        match exporter_kind("METRICS").as_str() {
            "otlp" => {
                let exporter = opentelemetry_otlp::MetricExporter::builder()
                    .with_http()
                    .build();
                if let Some(exporter) = report("metrics", exporter) {
                    metrics = metrics.with_reader(PeriodicReader::builder(exporter).build());
                }
            }
            "console" => {
                metrics = metrics.with_reader(
                    PeriodicReader::builder(opentelemetry_stdout::MetricExporter::default())
                        .build(),
                );
            }
            _ => {}
        }

        let mut logs = SdkLoggerProvider::builder().with_resource(resource);
        match exporter_kind("LOGS").as_str() {
            "otlp" => {
                let exporter = opentelemetry_otlp::LogExporter::builder()
                    .with_http()
                    .build();
                if let Some(exporter) = report("logs", exporter) {
                    logs = logs.with_batch_exporter(exporter);
                }
            }
            "console" => {
                logs = logs.with_simple_exporter(opentelemetry_stdout::LogExporter::default());
            }
            _ => {}
        }

        let telemetry = Telemetry {
            tracer_provider: traces.build(),
            meter_provider: metrics.build(),
            logger_provider: logs.build(),
        };
        opentelemetry::global::set_text_map_propagator(TraceContextPropagator::new());
        opentelemetry::global::set_tracer_provider(telemetry.tracer_provider.clone());
        opentelemetry::global::set_meter_provider(telemetry.meter_provider.clone());

        // RUST_LOG filters the logs (stdout and OTel); the exported spans are the
        // `otel::tracing` ones whatever RUST_LOG says.
        let rust_log = std::env::var("RUST_LOG")
            .unwrap_or_else(|_| "irisqo=debug,tower_http=debug,sqlx=info".into());
        let log_filter = EnvFilter::new(&rust_log);
        let span_filter = Targets::new().with_target(TRACING_TARGET, Level::INFO);
        let log_export_filter = EnvFilter::new(format!("{rust_log},{LOG_EXPORT_DENY}"));
        tracing_subscriber::registry()
            .with(tracing_subscriber::fmt::layer().with_filter(log_filter))
            .with(
                tracing_opentelemetry::layer()
                    .with_tracer(telemetry.tracer_provider.tracer("irisqo"))
                    .with_filter(span_filter),
            )
            .with(
                OpenTelemetryTracingBridge::new(&telemetry.logger_provider)
                    .with_filter(log_export_filter),
            )
            .init();
        telemetry
    }

    /// Flushes and stops the exporters.
    pub fn shutdown(&self) {
        let results = [
            ("traces", self.tracer_provider.shutdown()),
            ("metrics", self.meter_provider.shutdown()),
            ("logs", self.logger_provider.shutdown()),
        ];
        for (signal, res) in results {
            if let Err(err) = res {
                eprintln!("telemetry {signal} shutdown error {err:?}");
            }
        }
    }
}
