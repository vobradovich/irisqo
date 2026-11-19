//! `--max-body-size`: the routes that take a job refuse a request body above the limit, the rest do not.

use std::convert::Infallible;

use axum::{body::Body, http::Request};
use bytes::Bytes;
use hyper::StatusCode;
use sqlx::PgPool;
use tower_service::Service;

use crate::common;

const LIMIT: usize = 128;

#[sqlx::test(migrations = "./sql/migrations")]
async fn job_submission_limits_the_entire_request_body(pool: PgPool) -> anyhow::Result<()> {
    let state = common::state_with(pool, &["--max-body-size", &LIMIT.to_string()]);
    let mut router = irisqo::app(&state);

    // an invalid submission reaches its handler at the limit, a byte more is refused
    for (method, path, at_limit) in [
        ("POST", "/api/v1/jobs", StatusCode::BAD_REQUEST),
        ("PUT", "/api/v1/schedules/nightly", StatusCode::BAD_REQUEST),
        ("POST", "/to/invalid", StatusCode::BAD_REQUEST),
        ("POST", "/ui/jobs", StatusCode::UNPROCESSABLE_ENTITY),
    ] {
        for (size, status) in [
            (LIMIT, at_limit),
            (LIMIT + 1, StatusCode::PAYLOAD_TOO_LARGE),
        ] {
            // no Content-Length: the limit counts the bytes read
            let chunks = [Bytes::from(vec![b' '; size - 1]), Bytes::from_static(b"!")];
            let request = Request::builder()
                .method(method)
                .uri(path)
                .body(Body::from_stream(futures::stream::iter(
                    chunks.into_iter().map(Ok::<_, Infallible>),
                )))?;
            assert_eq!(router.call(request).await?.status(), status, "{path}");
        }
    }

    // only the submissions are limited
    let echo = Request::post("/echo")
        .header("content-length", LIMIT + 1)
        .body(Body::from(vec![b'!'; LIMIT + 1]))?;
    assert_eq!(router.call(echo).await?.status(), StatusCode::OK);
    Ok(())
}

/// `--max-response-size`: a successful response longer than the limit fails the job (it is not
/// retried, a callback tells, the headers are kept), one of the limit does not. The status of a
/// failure decides as ever: a 5xx is retried, a 4xx is final. `0` takes no body at all.
#[sqlx::test(migrations = "./sql/migrations")]
async fn a_long_response_fails_the_job(pool: PgPool) -> anyhow::Result<()> {
    use hyper::Method;

    let state = common::state_with(pool.clone(), &["--max-response-size", "100"]);
    let addr = common::spawn_app(&state).await;
    let callback = format!("http%3A%2F%2F{addr}%2Fecho");
    let submit = |query: String, size: usize| {
        let (state, pool) = (state.clone(), pool.clone());
        async move {
            let response = common::send(
                &state,
                Method::POST,
                &format!("http://{addr}/to/http://{addr}/echo?{query}"),
                &[("content-type", "text/plain")],
                &"x".repeat(size),
            )
            .await;
            assert_eq!(response.status(), StatusCode::CREATED, "{query}");
            let id: i64 = common::header(&response, "irisqo-job-id")
                .and_then(|v| v.parse().ok())
                .expect("job id");
            // callbacks of the jobs before may be waiting too
            assert!(common::run_enqueued(&state).await >= 1, "{query}");
            let processed = sqlx::query_as::<_, (String, i32, Option<String>, i64)>(
                "SELECT status, attempt, split_part(outcome->>'error', ':', 1), coalesce(length(body), 0)::bigint
                 FROM processed WHERE id = $1",
            )
            .bind(id)
            .fetch_one(&pool)
            .await
            .expect("processed");
            let callbacks: Vec<String> =
                sqlx::query_scalar("SELECT meta->>'callback_as' FROM jobs WHERE parent_id = $1")
                    .bind(id)
                    .fetch_all(&pool)
                    .await
                    .expect("callbacks");
            (processed, callbacks)
        }
    };
    let too_large = |attempt: i32| {
        (
            "failed".to_owned(),
            attempt,
            Some("response too large".to_owned()),
            0,
        )
    };

    // the limit itself passes, a byte more fails
    let ((status, _, error, length), _) = submit("_x=1".into(), 100).await;
    assert_eq!((status.as_str(), error, length), ("completed", None, 100));
    let (processed, _) = submit("_x=1".into(), 101).await;
    assert_eq!(processed, too_large(0));

    // the status and the headers of the response are kept, the body is not
    let (status, error): (Option<i32>, Option<String>) = sqlx::query_as(
        "SELECT (outcome->>'status_code')::int, outcome->>'error'
         FROM processed ORDER BY id DESC LIMIT 1",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(status, Some(200));
    assert_eq!(
        error.as_deref(),
        Some("response too large: 101 bytes, the limit is 100")
    );
    let headers: Option<serde_json::Value> =
        sqlx::query_scalar("SELECT headers FROM processed ORDER BY id DESC LIMIT 1")
            .fetch_one(&pool)
            .await?;
    assert!(headers.is_some_and(|h| h.get("date").is_some()));

    // a callback of any mode tells the job failed (a relay sends completed ones by default)
    let (processed, made) = submit(format!("_callback_url={callback}"), 300).await;
    assert_eq!(processed, too_large(0));
    assert_eq!(made, ["webhook"]);
    let (processed, made) = submit(
        format!("_callback_url={callback}&_callback_mode=relay"),
        300,
    )
    .await;
    assert_eq!(processed, too_large(0));
    assert!(made.is_empty());

    // a 4xx is final as ever, with its status and no body
    let (processed, _) = submit("status=404".into(), 300).await;
    assert_eq!(processed, ("failed".to_owned(), 0, None, 0));
    let status: Option<i32> = sqlx::query_scalar(
        "SELECT (outcome->>'status_code')::int FROM processed ORDER BY id DESC LIMIT 1",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(status, Some(404));

    // a 5xx is retried by the policy of the job: its body does not matter
    let response = common::send(
        &state,
        hyper::Method::POST,
        &format!("http://{addr}/to/http://{addr}/echo?status=503&_retry=2"),
        &[("content-type", "text/plain")],
        &"x".repeat(300),
    )
    .await;
    let id: i64 = common::header(&response, "irisqo-job-id")
        .and_then(|v| v.parse().ok())
        .expect("job id");
    assert_eq!(common::run_enqueued(&state).await, 1);
    assert_eq!(common::processed_status(&pool, id).await, None);
    let waiting: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM (SELECT id FROM queue WHERE state = 'scheduled' AND id = $1 AND attempt = 1
         UNION ALL SELECT id FROM queue WHERE state = 'ready' AND id = $1 AND attempt = 1) AS w",
    )
    .bind(id)
    .fetch_one(&pool)
    .await?;
    assert_eq!(waiting, 1, "the 503 is waiting for its retry");

    // `0` takes no body: an empty response passes
    let state = common::state_with(pool.clone(), &["--max-response-size", "0"]);
    let addr = common::spawn_app(&state).await;
    for (size, status) in [(0, "completed"), (1, "failed")] {
        let response = common::send(
            &state,
            Method::POST,
            &format!("http://{addr}/to/http://{addr}/echo"),
            &[("content-type", "text/plain")],
            &"x".repeat(size),
        )
        .await;
        let id: i64 = common::header(&response, "irisqo-job-id")
            .and_then(|v| v.parse().ok())
            .expect("job id");
        // the retry of the 503 is waiting too
        assert!(common::run_enqueued(&state).await >= 1);
        assert_eq!(
            common::processed_status(&pool, id).await.as_deref(),
            Some(status),
            "{size}"
        );
    }
    Ok(())
}
