use crate::models::{AppState, Error, Problem};
use axum::{
    Json, Router,
    body::Body,
    extract::{Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::IntoResponse,
    routing::get,
};
use hyper::Method;

use std::{collections::HashMap, sync::Arc, time::Duration};
#[allow(unused_imports)]
use tracing::{debug, error, info, warn};

pub fn routes(state: Arc<AppState>) -> Router {
    Router::new()
        .route(
            "/echo",
            get(echo).post(echo).put(echo).patch(echo).delete(echo),
        )
        .route(
            "/error",
            get(error).post(error).put(error).patch(error).delete(error),
        )
        .route("/bench", get(bench).post(bench))
        .with_state(state)
}

/// Benchmark of the queue: puts `n` jobs (default 20000, `?n=1000`) that call `/echo` of this
/// server into the queue, waits until they are processed and answers with the time
/// (`insert_ms`, `seconds`, `jobs_per_s`, `completed`, `failed`). `?sleep_ms=` delays every call,
/// `?timeout_s=` (default 300) stops the wait. Development only, like `/echo`.
async fn bench(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Json<serde_json::Value>, Problem> {
    let n: i32 = params
        .get("n")
        .and_then(|v| v.parse().ok())
        .unwrap_or(20_000);
    let timeout = Duration::from_secs(
        params
            .get("timeout_s")
            .and_then(|v| v.parse().ok())
            .unwrap_or(300),
    );
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| Error::invalid("host", "the Host header is needed to call /echo"))?;
    let mut url = format!("http://{host}/echo");
    if let Some(ms) = params.get("sleep_ms") {
        url = format!("{url}?sleep_ms={ms}");
    }

    let started = std::time::Instant::now();
    const INSERT: &str = "
    WITH a AS (
        INSERT INTO jobs(meta, headers, body)
        SELECT jsonb_build_object('retry', 'none', 'timeout', 10000, 'protocol', 'http', 'method', 'GET', 'url', $1::text),
               '{}'::jsonb, NULL
        FROM generate_series(1, $2)
        RETURNING id
    ), e AS (
        INSERT INTO enqueued(id) SELECT id FROM a
    )
    SELECT min(id) FROM a";
    let first: Option<i64> = sqlx::query_scalar(INSERT)
        .bind(&url)
        .bind(n)
        .fetch_one(&state.pool)
        .await
        .map_err(|e| Problem::from(Error::from(e)))?;
    let first = first.unwrap_or(0);
    let insert_ms = started.elapsed().as_secs_f64() * 1000.0;

    // the jobs run as soon as they are in the queue
    let run = std::time::Instant::now();
    let (mut done, mut failed) = (0_i64, 0_i64);
    while done < i64::from(n) && run.elapsed() < timeout {
        (done, failed) = sqlx::query_as(
            "SELECT count(*), count(*) FILTER (WHERE status = 'failed') FROM processed WHERE id >= $1",
        )
        .bind(first)
        .fetch_one(&state.pool)
        .await
        .map_err(|e| Problem::from(Error::from(e)))?;
        if done < i64::from(n) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    let seconds = run.elapsed().as_secs_f64();
    Ok(Json(serde_json::json!({
        "jobs": n,
        "insert_ms": (insert_ms * 10.0).round() / 10.0,
        "seconds": (seconds * 1000.0).round() / 1000.0,
        "jobs_per_s": (f64::from(n) / seconds).round(),
        "completed": done - failed,
        "failed": failed,
        "finished": done >= i64::from(n),
    })))
}

/// Echoes the body and `content*` headers back. For tests:
/// - `?status=429&retry-after=120` set the response status and `Retry-After`;
/// - `?headers=1` answers with the request headers as a JSON object instead of the body;
/// - `?sleep_ms=500` answers after a delay.
async fn echo(
    method: Method,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: Body,
) -> impl IntoResponse {
    if let Some(ms) = params.get("sleep_ms").and_then(|ms| ms.parse().ok()) {
        tokio::time::sleep(Duration::from_millis(ms)).await;
    }
    let mut response = if params.contains_key("headers") {
        let received: HashMap<&str, &str> = headers
            .iter()
            .filter_map(|(k, v)| Some((k.as_str(), v.to_str().ok()?)))
            .collect();
        Json(received).into_response()
    } else {
        body.into_response()
    };
    if let Some(status) = params
        .get("status")
        .and_then(|s| s.parse::<StatusCode>().ok())
    {
        *response.status_mut() = status;
    }
    let response_headers = response.headers_mut();
    if let Some(value) = params
        .get("retry-after")
        .and_then(|v| HeaderValue::from_str(v).ok())
    {
        response_headers.insert(header::RETRY_AFTER, value);
    }
    if let Ok(value) = HeaderValue::from_str(method.as_str()) {
        response_headers.insert("http-method", value);
    }
    if !params.contains_key("headers") {
        for (k, v) in headers {
            if let Some(key) = k
                && key.as_str().starts_with("content")
            {
                response_headers.insert(key, v);
            }
        }
    }
    response
}

async fn error() -> Problem {
    Problem::new(StatusCode::INTERNAL_SERVER_ERROR)
}
