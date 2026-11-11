use std::{net::SocketAddr, sync::Arc};

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use irisqo::{
    AppState, db,
    models::{JobWithRetry, Options, new_instance_id},
    services::jobrunner,
};
use sqlx::PgPool;
use tokio::net::TcpListener;

/// App state for tests: default options on top of the per-test pool.
pub fn state(pool: PgPool) -> Arc<AppState> {
    state_with(pool, &[])
}

/// App state for tests with command line `args`, e.g. `["--shutdown-timeout", "1"]`.
/// The environment is development (for `/echo` and the local targets) unless `args` set one.
pub fn state_with(pool: PgPool, args: &[&str]) -> Arc<AppState> {
    crate::telemetry::telemetry();
    let mut args: Vec<_> = args.iter().map(Into::into).collect();
    if !args.iter().any(|arg| arg == "--env") {
        args.extend(["--env".into(), "development".into()]);
    }
    let options = Options::from_vec(args).expect("options");
    AppState::with_pool(pool, new_instance_id(), &options)
}

/// Serves the app (API, UI, and `/echo`, `/error` in development) on a random local port.
pub async fn spawn_app(state: &Arc<AppState>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    let app = irisqo::app(state);
    tokio::spawn(async move {
        axum::serve(listener, app.into_make_service())
            .await
            .expect("serve");
    });
    addr
}

/// Sends a request through the app's own HTTP client.
pub async fn request(
    state: &AppState,
    method: hyper::Method,
    uri: &str,
) -> hyper::Response<hyper::body::Incoming> {
    request_with(state, method, uri, &[]).await
}

/// [`request`] with `headers`.
pub async fn request_with(
    state: &AppState,
    method: hyper::Method,
    uri: &str,
    headers: &[(&str, &str)],
) -> hyper::Response<hyper::body::Incoming> {
    send(state, method, uri, headers, "").await
}

/// [`request_with`] and a `body`: a form (`application/x-www-form-urlencoded`) unless `headers`
/// bring their own `content-type`.
pub async fn send(
    state: &AppState,
    method: hyper::Method,
    uri: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> hyper::Response<hyper::body::Incoming> {
    let mut req = hyper::Request::builder().method(method).uri(uri);
    if !body.is_empty()
        && !headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("content-type"))
    {
        req = req.header("content-type", "application/x-www-form-urlencoded");
    }
    for (name, value) in headers {
        req = req.header(*name, *value);
    }
    let req = req
        .body(Full::new(Bytes::copy_from_slice(body.as_bytes())))
        .expect("request");
    state.client.request(req).await.expect("response")
}

/// The body of a response as text.
pub async fn text(response: hyper::Response<hyper::body::Incoming>) -> String {
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Assigns up to `limit` enqueued jobs to this instance.
pub async fn fetch(state: &AppState, limit: i32) -> Vec<JobWithRetry> {
    db::jobqueue::fetch_job_with_retry(&state.pool, &state.instance_id, limit)
        .await
        .expect("fetch_job_with_retry")
}

/// Fetches and runs every enqueued job once; returns how many ran.
pub async fn run_enqueued(state: &AppState) -> usize {
    let jobs = fetch(state, 1000).await;
    let count = jobs.len();
    for job in jobs {
        jobrunner::job_run(state, job).await;
    }
    count
}

/// `processed.status` of a job, `None` while it is not processed.
pub async fn processed_status(pool: &PgPool, job_id: i64) -> Option<String> {
    sqlx::query_scalar("SELECT status FROM processed WHERE id = $1")
        .bind(job_id)
        .fetch_optional(pool)
        .await
        .expect("processed status")
}

/// Creates a job through the API (`/to/...`) and returns the response.
pub async fn create(
    state: &AppState,
    addr: SocketAddr,
    target: &str,
) -> hyper::Response<hyper::body::Incoming> {
    request(
        state,
        hyper::Method::POST,
        &format!("http://{addr}/to/{target}"),
    )
    .await
}

/// A response header value as a string.
pub fn header(response: &hyper::Response<hyper::body::Incoming>, name: &str) -> Option<String> {
    response
        .headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

/// Makes every scheduled job due and moves it to `enqueued`.
pub async fn enqueue_all_scheduled(state: &AppState) -> u64 {
    sqlx::query("UPDATE scheduled SET at = to_timestamp(1)")
        .execute(&state.pool)
        .await
        .expect("make scheduled due");
    db::jobqueue::enqueue_scheduled(&state.pool, &state.instance_id, 10_000)
        .await
        .expect("enqueue_scheduled")
}

pub async fn count(pool: &PgPool, sql: &'static str) -> i64 {
    sqlx::query_scalar(sql)
        .fetch_one(pool)
        .await
        .expect("count")
}

/// Inserts `n` noop jobs straight into `enqueued`; returns their ids.
pub async fn insert_enqueued(pool: &PgPool, n: i32) -> Vec<i64> {
    sqlx::query_scalar(
        "WITH a AS (
            INSERT INTO jobs(meta) SELECT '{\"sink\": \"noop\", \"retry\": \"none\", \"timeout\": 1000}'::jsonb
            FROM generate_series(1, $1) RETURNING id
        )
        INSERT INTO enqueued(id) SELECT id FROM a RETURNING id",
    )
    .bind(n)
    .fetch_all(pool)
    .await
    .expect("insert enqueued")
}

/// Statuses of a job's history, oldest first.
pub async fn history(pool: &PgPool, job_id: i64) -> Vec<(String, Option<String>)> {
    sqlx::query_as("SELECT kind, message FROM history WHERE id = $1 ORDER BY seq")
        .bind(job_id)
        .fetch_all(pool)
        .await
        .expect("history")
}
