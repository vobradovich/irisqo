use std::{collections::HashMap, sync::Arc, time::Duration};

use bytes::Bytes;
use http_body_util::Full;
use hyper::StatusCode;
use irisqo::{db, services::workerservice::WorkerService};
use sqlx::PgPool;

use crate::common;

/// Request headers the target got, from a job to `/echo?headers=1`.
async fn received_headers(pool: &PgPool, job_id: i64) -> HashMap<String, String> {
    let body: Vec<u8> = sqlx::query_scalar("SELECT body FROM results WHERE id = $1")
        .bind(job_id)
        .fetch_one(pool)
        .await
        .expect("processed body");
    serde_json::from_slice(&body).expect("headers json")
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn only_allowed_headers_are_forwarded(pool: PgPool) -> anyhow::Result<()> {
    let state = common::state(pool.clone());
    let addr = common::spawn_app(&state).await;
    let req = hyper::Request::post(format!("http://{addr}/to/http://{addr}/echo?headers=1"))
        .header("authorization", "Bearer caller")
        .header("cookie", "session=caller")
        .header("x-custom", "dropped")
        .header("irisqo-forward-authorization", "Bearer target")
        .header("irisqo-forward-x-custom", "kept")
        .header("content-type", "text/plain")
        .body(Full::new(Bytes::from_static(b"hello")))?;
    let response = state.client.request(req).await?;
    assert_eq!(response.status(), StatusCode::CREATED);
    let job_id: i64 = common::header(&response, "job-id")
        .and_then(|v| v.parse().ok())
        .expect("job-id");

    common::run_enqueued(&state).await;

    let got = received_headers(&pool, job_id).await;
    assert_eq!(
        got.get("authorization").map(String::as_str),
        Some("Bearer target")
    );
    assert_eq!(got.get("x-custom").map(String::as_str), Some("kept"));
    assert_eq!(
        got.get("content-type").map(String::as_str),
        Some("text/plain")
    );
    assert!(!got.contains_key("cookie"));
    assert!(!got.keys().any(|k| k.starts_with("irisqo-forward-")));
    assert_eq!(got.get("irisqo-job-id"), Some(&job_id.to_string()));
    assert_eq!(got.get("irisqo-retry").map(String::as_str), Some("0"));
    assert!(!got.contains_key("irisqo-schedule-id"));
    Ok(())
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn redelivery_has_the_same_job_id_and_the_next_retry(pool: PgPool) -> anyhow::Result<()> {
    let state = common::state(pool.clone());
    let addr = common::spawn_app(&state).await;
    let response = common::create(
        &state,
        addr,
        &format!("http://{addr}/echo?headers=1&status=503&_retry=1"),
    )
    .await;
    let job_id: i64 = common::header(&response, "job-id")
        .and_then(|v| v.parse().ok())
        .expect("job-id");

    // first attempt: 503, retried at once; second one: 503 again, failed
    assert_eq!(common::run_enqueued(&state).await, 1);
    assert_eq!(common::run_enqueued(&state).await, 1);

    let got = received_headers(&pool, job_id).await;
    assert_eq!(got.get("irisqo-job-id"), Some(&job_id.to_string()));
    assert_eq!(got.get("irisqo-retry").map(String::as_str), Some("1"));

    // the history says why each attempt ended
    let history = common::history(&pool, job_id).await;
    let steps: Vec<_> = history
        .iter()
        .map(|(kind, message)| (kind.as_str(), message.as_deref()))
        .collect();
    assert_eq!(
        steps,
        [
            ("enqueued", None),
            ("claimed", None),
            ("retried", Some("HTTP 503")),
            ("claimed", None),
            ("failed", Some("HTTP 503")),
        ]
    );
    Ok(())
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn history_keeps_the_error_of_a_failed_connection(pool: PgPool) -> anyhow::Result<()> {
    let state = common::state(pool.clone());
    let addr = common::spawn_app(&state).await;
    // nothing listens on port 9
    let response = common::create(&state, addr, "http://127.0.0.1:9/hook").await;
    let job_id: i64 = common::header(&response, "job-id")
        .and_then(|v| v.parse().ok())
        .expect("job-id");
    assert_eq!(common::run_enqueued(&state).await, 1);

    let (kind, message) = common::history(&pool, job_id).await.pop().expect("history");
    assert_eq!(kind, "failed");
    assert!(
        message.unwrap_or_default().contains("Connect"),
        "the connect error is in the history"
    );
    Ok(())
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn schedule_run_has_its_schedule_id(pool: PgPool) -> anyhow::Result<()> {
    let state = common::state(pool.clone());
    let addr = common::spawn_app(&state).await;
    let response = common::create(
        &state,
        addr,
        &format!("http://{addr}/echo?headers=1&_interval=60"),
    )
    .await;
    let job_id: i64 = common::header(&response, "job-id")
        .and_then(|v| v.parse().ok())
        .expect("job-id");
    let schedule_id = common::header(&response, "schedule-id").expect("schedule-id");
    common::enqueue_all_scheduled(&state).await;
    common::run_enqueued(&state).await;

    let got = received_headers(&pool, job_id).await;
    assert_eq!(got.get("irisqo-schedule-id"), Some(&schedule_id));
    Ok(())
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn shutdown_timeout_abandons_running_jobs(pool: PgPool) -> anyhow::Result<()> {
    let state = common::state_with(pool.clone(), &["--shutdown-timeout", "1"]);
    let addr = common::spawn_app(&state).await;
    // a target that answers in 30 s
    let response = common::create(
        &state,
        addr,
        &format!("http://{addr}/echo?sleep_ms=30000&_timeout=60000"),
    )
    .await;
    let job_id: i64 = common::header(&response, "job-id")
        .and_then(|v| v.parse().ok())
        .expect("job-id");

    let worker = tokio::spawn({
        let state = Arc::clone(&state);
        async move { WorkerService::new(state).run().await }
    });
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while common::count(&pool, "SELECT count(*) FROM claimed").await == 0 {
        assert!(tokio::time::Instant::now() < deadline, "job not started");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    state.shutdown_token.cancel();
    // 1 s of shutdown timeout, not the 30 s of the job
    tokio::time::timeout(Duration::from_secs(5), worker).await??;
    assert_eq!(common::processed_status(&pool, job_id).await, None);

    // what main does on exit: the abandoned job runs again elsewhere, same attempt
    db::instances::kill(&pool, &state.instance_id).await?;
    let (retry, locked): (i32, bool) = sqlx::query_as(
        "SELECT retry, EXISTS (SELECT 1 FROM claimed WHERE id = $1) FROM enqueued WHERE id = $1",
    )
    .bind(job_id)
    .fetch_one(&pool)
    .await?;
    assert_eq!((retry, locked), (0, false));
    Ok(())
}
