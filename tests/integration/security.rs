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

#[sqlx::test(migrations = "./sql/migrations")]
async fn dev_endpoints_only_in_development(pool: PgPool) -> anyhow::Result<()> {
    for (env, expected) in [
        ("development", StatusCode::OK),
        ("staging", StatusCode::NOT_FOUND),
        ("production", StatusCode::NOT_FOUND),
    ] {
        let state = common::state_with(pool.clone(), &["--env", env]);
        let addr = common::spawn_app(&state).await;
        let response = common::request(&state, Method::GET, &format!("http://{addr}/echo")).await;
        assert_eq!(response.status(), expected, "/echo in {env}");
        let response = common::request(&state, Method::GET, &format!("http://{addr}/error")).await;
        let expected = if expected == StatusCode::OK {
            StatusCode::INTERNAL_SERVER_ERROR
        } else {
            expected
        };
        assert_eq!(response.status(), expected, "/error in {env}");
    }
    Ok(())
}

/// Creates a job through the API of `state`; returns the status and the body.
async fn create(
    state: &irisqo::AppState,
    addr: std::net::SocketAddr,
    target: &str,
) -> (StatusCode, serde_json::Value) {
    let response = common::create(state, addr, target).await;
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or_default())
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn private_targets_are_refused_outside_development(pool: PgPool) -> anyhow::Result<()> {
    let prod = common::state_with(pool.clone(), &["--env", "production"]);
    let addr = common::spawn_app(&prod).await;
    for target in [
        "http://127.0.0.1:8080/hook",
        "http://169.254.169.254/latest/meta-data/",
        "http://[::1]/hook",
        "http://localhost/hook",
        "http://10.0.0.7/hook",
    ] {
        let (status, body) = create(&prod, addr, target).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{target}");
        assert_eq!(body["errors"][0]["parameter"], "url", "{target}: {body}");
        assert_eq!(
            body["errors"][0]["detail"], "is a private address",
            "{target}"
        );
    }
    let (status, _) = create(&prod, addr, "https://example.com/hook?_delay=600").await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "a public name is checked when resolved"
    );
    Ok(())
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn worker_refuses_private_targets(pool: PgPool) -> anyhow::Result<()> {
    // jobs made where they were allowed (or before the check existed), run in production
    let dev = common::state(pool.clone());
    let dev_addr = common::spawn_app(&dev).await;
    let port = dev_addr.port();
    let mut ids = Vec::new();
    for target in [
        format!("http://localhost:{port}/echo"),
        format!("http://127.0.0.1:{port}/echo"),
    ] {
        let response = common::create(&dev, dev_addr, &target).await;
        ids.push(
            common::header(&response, "job-id")
                .expect("job-id")
                .parse::<i64>()?,
        );
    }

    let prod = common::state_with(pool.clone(), &["--env", "production"]);
    assert_eq!(common::run_enqueued(&prod).await, 2);
    for id in ids {
        assert_eq!(
            common::processed_status(&pool, id).await.as_deref(),
            Some("failed"),
            "job {id}: no retry"
        );
        let response = common::request(
            &dev,
            Method::GET,
            &format!("http://{dev_addr}/api/v1/jobs/{id}/result"),
        )
        .await;
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        let result = String::from_utf8_lossy(&bytes);
        assert!(result.contains("Blocked target"), "job {id}: {result}");
    }
    Ok(())
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn allowed_targets_pass(pool: PgPool) -> anyhow::Result<()> {
    // the target: /echo of a development instance
    let dev = common::state(pool.clone());
    let dev_addr = common::spawn_app(&dev).await;

    let prod = common::state_with(
        pool.clone(),
        &["--env", "production", "--allow-targets", "127.0.0.0/8"],
    );
    let addr = common::spawn_app(&prod).await;
    let (status, _) = create(
        &prod,
        addr,
        &format!("http://localhost:{}/echo", dev_addr.port()),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(common::run_enqueued(&prod).await, 1);
    let completed = common::count(
        &pool,
        "SELECT count(*) FROM processed WHERE status = 'completed'",
    )
    .await;
    assert_eq!(completed, 1);
    Ok(())
}
