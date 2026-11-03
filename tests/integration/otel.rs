use std::{sync::OnceLock, time::Duration};

use bytes::Bytes;
use http_body_util::Full;
use hyper::StatusCode;
use opentelemetry::{
    Value,
    trace::{SpanId, TracerProvider as _},
};
use opentelemetry_sdk::{
    propagation::TraceContextPropagator,
    trace::{InMemorySpanExporter, SdkTracerProvider, SpanData},
};
use sqlx::PgPool;
use tracing_subscriber::layer::SubscriberExt;

use crate::common;

/// One in-memory exporter for the whole test binary (the tracing subscriber is global);
/// every test filters the spans by its own trace id.
fn exporter() -> &'static InMemorySpanExporter {
    static EXPORTER: OnceLock<InMemorySpanExporter> = OnceLock::new();
    EXPORTER.get_or_init(|| {
        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        opentelemetry::global::set_text_map_propagator(TraceContextPropagator::new());
        opentelemetry::global::set_tracer_provider(provider.clone());
        let subscriber = tracing_subscriber::registry()
            .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("test")));
        tracing::subscriber::set_global_default(subscriber).expect("global subscriber");
        exporter
    })
}

fn random_trace_id() -> String {
    format!("{:032x}", fastrand::u128(1..))
}

/// `POST /to/{target}` with a client `traceparent` of `trace_id`.
async fn create_traced(
    state: &irisqo::AppState,
    addr: std::net::SocketAddr,
    target: &str,
    trace_id: &str,
) -> i64 {
    let req = hyper::Request::post(format!("http://{addr}/to/{target}"))
        .header("traceparent", format!("00-{trace_id}-00f067aa0ba902b7-01"))
        .body(Full::new(Bytes::new()))
        .expect("request");
    let response = state.client.request(req).await.expect("response");
    assert_eq!(response.status(), StatusCode::CREATED);
    common::header(&response, "job-id")
        .and_then(|v| v.parse().ok())
        .expect("job-id")
}

fn spans_of(trace_id: &str) -> Vec<SpanData> {
    exporter()
        .get_finished_spans()
        .expect("spans")
        .into_iter()
        .filter(|s| s.span_context.trace_id().to_string() == trace_id)
        .collect()
}

/// Waits for the spans that end after the response (server spans).
async fn wait_spans(trace_id: &str, name: &str, count: usize) -> Vec<SpanData> {
    for _ in 0..100 {
        let spans: Vec<_> = spans_of(trace_id)
            .into_iter()
            .filter(|s| s.name == name)
            .collect();
        if spans.len() >= count {
            return spans;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("no {count} '{name}' spans in trace {trace_id}");
}

fn attr(span: &SpanData, key: &str) -> Option<Value> {
    span.attributes
        .iter()
        .find(|kv| kv.key.as_str() == key)
        .map(|kv| kv.value.clone())
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn job_continues_the_request_trace(pool: PgPool) -> anyhow::Result<()> {
    exporter();
    let state = common::state(pool.clone());
    let addr = common::spawn_app(&state).await;
    let trace_id = random_trace_id();
    let job_id = create_traced(&state, addr, &format!("http://{addr}/echo"), &trace_id).await;
    let (traceparent, stored_header): (Option<String>, bool) =
        sqlx::query_as("SELECT meta->>'traceparent', headers ? 'traceparent' FROM jobs")
            .fetch_one(&pool)
            .await?;
    assert!(traceparent.expect("traceparent").contains(&trace_id));
    assert!(
        !stored_header,
        "the client traceparent is not replayed as a header"
    );

    common::run_enqueued(&state).await;

    // job.run -> client span -> the receiver's server span, all in the request trace
    let run = wait_spans(&trace_id, "job.run", 1).await.remove(0);
    let client = wait_spans(&trace_id, "POST", 1).await.remove(0);
    let receiver = wait_spans(&trace_id, "POST /echo", 1).await.remove(0);
    assert_eq!(client.parent_span_id, run.span_context.span_id());
    assert_eq!(receiver.parent_span_id, client.span_context.span_id());
    assert_eq!(attr(&run, "job.outcome"), Some("completed".into()));
    assert_eq!(
        attr(&receiver, "http.response.status_code"),
        Some(200_i64.into())
    );
    assert_eq!(
        attr(&client, "http.response.status_code"),
        Some(200_i64.into())
    );

    // the result links to the attempt
    let (result_trace, result_span): (Option<String>, Option<String>) =
        sqlx::query_as("SELECT meta->>'trace_id', meta->>'span_id' FROM processed WHERE id = $1")
            .bind(job_id)
            .fetch_one(&pool)
            .await?;
    assert_eq!(result_trace.as_deref(), Some(trace_id.as_str()));
    assert_eq!(result_span, Some(run.span_context.span_id().to_string()));
    Ok(())
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn retries_are_sibling_spans(pool: PgPool) -> anyhow::Result<()> {
    exporter();
    let state = common::state(pool.clone());
    let addr = common::spawn_app(&state).await;
    let trace_id = random_trace_id();
    create_traced(
        &state,
        addr,
        &format!("http://{addr}/echo?status=503&_retry=2"),
        &trace_id,
    )
    .await;

    for _ in 0..3 {
        assert_eq!(common::run_enqueued(&state).await, 1);
    }

    let mut runs = wait_spans(&trace_id, "job.run", 3).await;
    runs.sort_by_key(|s| s.start_time);
    let parent = runs[0].parent_span_id;
    assert!(runs.iter().all(|s| s.parent_span_id == parent));
    let outcomes: Vec<_> = runs.iter().map(|s| attr(s, "job.outcome")).collect();
    assert_eq!(
        outcomes,
        [
            Some("retry".into()),
            Some("retry".into()),
            Some("failed".into())
        ]
    );
    let retries: Vec<_> = runs.iter().map(|s| attr(s, "job.retry")).collect();
    assert_eq!(
        retries,
        [Some(0_i64.into()), Some(1_i64.into()), Some(2_i64.into())]
    );
    Ok(())
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn schedule_runs_start_linked_traces(pool: PgPool) -> anyhow::Result<()> {
    exporter();
    let state = common::state(pool.clone());
    let addr = common::spawn_app(&state).await;
    let trace_id = random_trace_id();
    create_traced(
        &state,
        addr,
        &format!("http://{addr}/echo?_interval=60"),
        &trace_id,
    )
    .await;

    // two runs: each one in its own new trace, linked to the request
    for _ in 0..2 {
        common::enqueue_all_scheduled(&state).await;
        assert_eq!(common::run_enqueued(&state).await, 1);
    }
    // the next run keeps the request context as is
    let traceparent: Option<String> = sqlx::query_scalar(
        "SELECT meta->>'traceparent' FROM jobs WHERE id IN (SELECT id FROM scheduled)",
    )
    .fetch_one(&pool)
    .await?;
    assert!(traceparent.expect("traceparent").contains(&trace_id));

    let mut linked = Vec::new();
    for _ in 0..100 {
        linked = exporter()
            .get_finished_spans()?
            .into_iter()
            .filter(|s| {
                s.name == "job.run"
                    && s.links
                        .iter()
                        .any(|l| l.span_context.trace_id().to_string() == trace_id)
            })
            .collect();
        if linked.len() >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(linked.len(), 2);
    assert!(linked.iter().all(|s| s.parent_span_id == SpanId::INVALID));
    assert_ne!(
        linked[0].span_context.trace_id(),
        linked[1].span_context.trace_id()
    );
    assert!(spans_of(&trace_id).iter().all(|s| s.name != "job.run"));
    Ok(())
}
