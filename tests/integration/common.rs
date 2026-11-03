use std::{net::SocketAddr, sync::Arc};

use bytes::Bytes;
use futures::TryStreamExt;
use http_body_util::Full;
use irisqo::{
    AppState, db,
    models::{JobWithRetry, Options, new_instance_id},
    services::jobrunner,
};
use sqlx::PgPool;
use tokio::net::TcpListener;

/// App state for tests: default options on top of the per-test pool.
pub fn state(pool: PgPool) -> Arc<AppState> {
    AppState::with_pool(pool, new_instance_id(), &Options::default())
}

/// Serves the app (API, `/echo`, `/error`) on a random local port.
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
    let req = hyper::Request::builder()
        .method(method)
        .uri(uri)
        .body(Full::new(Bytes::new()))
        .expect("request");
    state.client.request(req).await.expect("response")
}

/// Assigns up to `limit` enqueued jobs to this instance.
pub async fn fetch(state: &AppState, limit: i32) -> Vec<JobWithRetry> {
    db::jobqueue::fetch_job_with_retry(&state.pool, &state.instance_id, limit)
        .try_collect()
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
    sqlx::query_scalar("SELECT status::text FROM processed WHERE id = $1")
        .bind(job_id)
        .fetch_optional(pool)
        .await
        .expect("processed status")
}
