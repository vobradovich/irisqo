use http_body_util::BodyExt;
use hyper::{Method, StatusCode};
use sqlx::PgPool;

use crate::common;

/// Status, content type and the problem body of a response.
async fn problem(
    response: hyper::Response<hyper::body::Incoming>,
) -> (StatusCode, String, serde_json::Value) {
    let status = response.status();
    let content_type = common::header(&response, "content-type").unwrap_or_default();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    (
        status,
        content_type,
        serde_json::from_slice(&bytes).unwrap_or_default(),
    )
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn missing_objects_are_404(pool: PgPool) -> anyhow::Result<()> {
    let state = common::state(pool);
    let addr = common::spawn_app(&state).await;
    let pending = common::create(&state, addr, &format!("http://{addr}/echo?_delay=600")).await;
    let pending = common::header(&pending, "job-id").expect("job-id");

    for (path, detail) in [
        (
            "/api/v1/jobs/424242".to_owned(),
            "Job 424242 does not exist.",
        ),
        (
            format!("/api/v1/jobs/{pending}/result"),
            "has no result: it does not exist or is not processed yet.",
        ),
        (
            "/api/v1/jobs/424242/result/raw".to_owned(),
            "has no result: it does not exist or is not processed yet.",
        ),
        (
            "/api/v1/schedules/01JC0000000000000000000000".to_owned(),
            "Schedule 01JC0000000000000000000000 does not exist.",
        ),
    ] {
        let response = common::request(&state, Method::GET, &format!("http://{addr}{path}")).await;
        let (status, content_type, body) = problem(response).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
        assert_eq!(content_type, "application/problem+json", "{path}");
        assert_eq!(body["status"], 404, "{path}");
        assert!(
            body["detail"].as_str().is_some_and(|d| d.ends_with(detail)),
            "{path}: {body}"
        );
    }

    // an existing job is still there, and deleting a missing one stays 204 (idempotent)
    let response = common::request(
        &state,
        Method::GET,
        &format!("http://{addr}/api/v1/jobs/{pending}"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let response = common::request(
        &state,
        Method::DELETE,
        &format!("http://{addr}/api/v1/jobs/424242"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    Ok(())
}
