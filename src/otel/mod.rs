mod headerinjector;
pub mod metrics;
mod telemetry;
use std::borrow::Cow;
pub use telemetry::Telemetry;

use axum::extract::MatchedPath;
use axum::http;
pub use headerinjector::HeaderExtractor;
pub use headerinjector::HeaderInjector;
use hyper::Uri;
use hyper::Version;
use opentelemetry::Context;
use opentelemetry::trace::TraceContextExt;
use std::collections::HashMap;
use tower_http::classify::ServerErrorsFailureClass;
use tracing::field::Empty;
use tracing_opentelemetry::OpenTelemetrySpanExt;

pub const TRACING_TARGET: &str = "otel::tracing";

/// `traceparent` and `tracestate` of the current span, to be stored with a job.
#[must_use]
pub fn current_trace_headers() -> (Option<String>, Option<String>) {
    let mut carrier = HashMap::new();
    opentelemetry::global::get_text_map_propagator(|propagator| {
        propagator.inject_context(&current_context(), &mut carrier);
    });
    (
        carrier.remove("traceparent"),
        carrier.remove("tracestate").filter(|s| !s.is_empty()),
    )
}

/// Context of a stored `traceparent`/`tracestate`; an empty one when there is none.
#[must_use]
pub fn context_from_trace_headers(traceparent: Option<&str>, tracestate: Option<&str>) -> Context {
    let Some(traceparent) = traceparent else {
        return Context::new();
    };
    let mut carrier = HashMap::from([("traceparent".to_owned(), traceparent.to_owned())]);
    if let Some(tracestate) = tracestate {
        carrier.insert("tracestate".to_owned(), tracestate.to_owned());
    }
    opentelemetry::global::get_text_map_propagator(|propagator| propagator.extract(&carrier))
}

/// Trace and span ids of a span (for `processed.meta`); `None` when it is not traced.
#[must_use]
pub fn span_ids(span: &tracing::Span) -> Option<(String, String)> {
    let cx = span.context();
    let span_ref = cx.span();
    let span_context = span_ref.span_context();
    span_context.is_valid().then(|| {
        (
            span_context.trace_id().to_string(),
            span_context.span_id().to_string(),
        )
    })
}

#[inline]
#[must_use]
pub fn current_context() -> Context {
    tracing::Span::current().context()
}

#[inline]
#[must_use]
pub fn current_trace_id() -> Option<String> {
    span_ids(&tracing::Span::current()).map(|(trace_id, _)| trace_id)
}

pub fn make_span_from_request<B>(request: &http::Request<B>) -> tracing::Span {
    let parent = extract_context(request.headers());
    let http_method = request.method().as_str();
    let route = http_route(request);
    let span = tracing::info_span!(
        target: TRACING_TARGET,
        "HTTP request",
        http.request.method = %http_method,
        http.route = route,
        network.protocol.version = %http_flavor(request.version()),
        http.response.status_code = Empty, // to set on response
        url.path = request.uri().path(),
        url.query = request.uri().query(),
        url.scheme = ?url_scheme(request.uri()),
        otel.name = format!("{} {}", http_method, route),
        otel.kind = ?opentelemetry::trace::SpanKind::Server,
        otel.status_code = Empty, // to set on response
        trace_id = Empty, // to set on response
        request_id = Empty,
        exception.message = Empty, // to set on response
    );
    let _ = span.set_parent(parent);
    span
}

pub fn on_response<B>(
    response: &http::Response<B>,
    latency: std::time::Duration,
    span: &tracing::Span,
) {
    span.record(
        "http.response.status_code",
        i64::from(response.status().as_u16()),
    );
    // 5xx: `on_failure` sets ERROR; 4xx is the client's error, not the server's
    if response.status().is_server_error() {
        return;
    }
    span.record("otel.status_code", "OK");

    tracing::debug!(
        "finished processing request latency={} ms status={}",
        latency.as_millis(),
        response.status().as_u16(),
    );
}

pub fn on_failure(
    failure_classification: ServerErrorsFailureClass,
    latency: std::time::Duration,
    span: &tracing::Span,
) {
    match failure_classification {
        ServerErrorsFailureClass::StatusCode(status) if status.is_server_error() => {
            span.record("otel.status_code", "ERROR");
        }
        ServerErrorsFailureClass::Error(ref s) => {
            span.record("exception.message", s);
        }
        _ => {}
    }
    tracing::warn!(
        "request failed latency={} ms status={}",
        latency.as_millis(),
        failure_classification,
    );
}

#[must_use]
fn extract_context(headers: &http::HeaderMap) -> Context {
    let extractor = HeaderExtractor(headers);
    opentelemetry::global::get_text_map_propagator(|propagator| propagator.extract(&extractor))
}

#[inline]
fn http_route<B>(request: &http::Request<B>) -> &str {
    request
        .extensions()
        .get::<MatchedPath>()
        .map_or_else(|| "", |mp| mp.as_str())
}

#[inline]
#[must_use]
fn http_flavor(version: Version) -> Cow<'static, str> {
    match version {
        Version::HTTP_09 => "0.9".into(),
        Version::HTTP_10 => "1.0".into(),
        Version::HTTP_11 => "1.1".into(),
        Version::HTTP_2 => "2".into(),
        Version::HTTP_3 => "3".into(),
        other => format!("{other:?}").into(),
    }
}

#[inline]
fn url_scheme(uri: &Uri) -> &str {
    uri.scheme_str().unwrap_or_default()
}
