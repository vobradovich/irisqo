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
