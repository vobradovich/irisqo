use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Method, StatusCode};
use sqlx::PgPool;

use crate::common;

async fn send(
    state: &irisqo::AppState,
    method: Method,
    uri: &str,
    htmx: bool,
    body: &'static str,
) -> (StatusCode, hyper::HeaderMap, String) {
    let mut req = hyper::Request::builder().method(method).uri(uri);
    if htmx {
        req = req.header("hx-request", "true");
    }
    if !body.is_empty() {
        req = req.header("content-type", "application/x-www-form-urlencoded");
    }
    let response = state
        .client
        .request(
            req.body(Full::new(Bytes::from_static(body.as_bytes())))
                .expect("request"),
        )
        .await
        .expect("response");
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    (
        status,
        headers,
        String::from_utf8_lossy(&bytes).into_owned(),
    )
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn pages_use_only_embedded_assets(pool: PgPool) -> anyhow::Result<()> {
    let state = common::state(pool);
    let addr = common::spawn_app(&state).await;

    let (status, headers, _) =
        send(&state, Method::GET, &format!("http://{addr}/ui"), false, "").await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(
        headers.get("location").map(|v| v.as_bytes()),
        Some(&b"/ui/jobs/new"[..])
    );

    let (status, headers, html) = send(
        &state,
        Method::GET,
        &format!("http://{addr}/ui/jobs/new"),
        false,
        "",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let csp = headers
        .get("content-security-policy")
        .expect("csp")
        .to_str()?;
    assert!(csp.contains("script-src 'self'") && csp.contains("style-src 'self'"));
    assert!(
        !html.contains("src=\"http") && !html.contains("href=\"http"),
        "no CDN"
    );
    assert!(!html.contains("style=\""), "no inline styles");
    for asset in [
        "/ui/assets/htmx-2.0.8.min.js",
        "/ui/assets/pico-2.1.1.classless.min.css",
        "/ui/assets/app.css",
    ] {
        assert!(html.contains(asset), "{asset}");
        let (status, headers, body) = send(
            &state,
            Method::GET,
            &format!("http://{addr}{asset}"),
            false,
            "",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{asset}");
        assert!(headers.contains_key("cache-control"));
        assert!(!body.is_empty());
    }
    let (status, _, _) = send(
        &state,
        Method::GET,
        &format!("http://{addr}/ui/assets/nope.js"),
        false,
        "",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    Ok(())
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn form_creates_the_job(pool: PgPool) -> anyhow::Result<()> {
    let state = common::state(pool.clone());
    let addr = common::spawn_app(&state).await;

    let (status, headers, html) = send(
        &state,
        Method::POST,
        &format!("http://{addr}/ui/jobs"),
        true,
        "method=PUT&url=https%3A%2F%2Fexample.com%2Fhook&content_type=application%2Fjson&body=%7B%7D\
         &header_name=Authorization&header_value=Bearer+x&when=delay&delay=60&repeat=once\
         &retry=fixed&retry_count=3&retry_delay=5&timeout=2000&external_id=order-1",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers.get("hx-trigger").map(|v| v.as_bytes()),
        Some(&b"job-created"[..])
    );
    assert!(html.contains("queued"), "{html}");
    assert!(
        html.starts_with("<div id=\"job-new\""),
        "a partial for htmx"
    );

    let (method, retry, timeout, auth, external_id): (String, String, i64, String, String) = sqlx::query_as(
        "SELECT meta->>'method', meta->>'retry', (meta->>'timeout')::bigint, headers->>'authorization', external_id FROM jobs",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        (method.as_str(), retry.as_str(), timeout),
        ("PUT", "fixed", 2000)
    );
    assert_eq!(
        (auth.as_str(), external_id.as_str()),
        ("Bearer x", "order-1")
    );
    assert_eq!(
        common::count(&pool, "SELECT count(*) FROM scheduled").await,
        1
    );
    Ok(())
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn form_errors_are_shown_by_field(pool: PgPool) -> anyhow::Result<()> {
    let state = common::state(pool.clone());
    let addr = common::spawn_app(&state).await;
    let body = "method=POST&url=https%3A%2F%2Fexample.com&repeat=cron&cron=*%2F15+*+*+*&when=now&retry=none";

    let (status, headers, html) = send(
        &state,
        Method::POST,
        &format!("http://{addr}/ui/jobs"),
        true,
        body,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(!headers.contains_key("hx-trigger"));
    assert!(html.contains("needs 5 or 6 fields, got 4"), "{html}");
    assert!(html.contains("aria-describedby=\"cron-error\""));
    assert!(
        html.contains("value=\"*/15 * * *\""),
        "the value is kept as typed"
    );

    // without htmx: the whole page, 422
    let (status, _, html) = send(
        &state,
        Method::POST,
        &format!("http://{addr}/ui/jobs"),
        false,
        body,
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(html.contains("<html"));
    assert_eq!(common::count(&pool, "SELECT count(*) FROM jobs").await, 0);
    Ok(())
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn choices_rerender_the_form(pool: PgPool) -> anyhow::Result<()> {
    let state = common::state(pool);
    let addr = common::spawn_app(&state).await;
    let get = |query: &str| format!("http://{addr}/ui/jobs/new/form?{query}");

    let (_, _, html) = send(
        &state,
        Method::GET,
        &get("when=now&repeat=once&retry=none"),
        true,
        "",
    )
    .await;
    assert!(!html.contains("id=\"delay\"") && !html.contains("id=\"cron\""));
    let (_, _, html) = send(
        &state,
        Method::GET,
        &get("when=delay&repeat=cron&retry=fibonacci"),
        true,
        "",
    )
    .await;
    assert!(
        html.contains("id=\"delay\"")
            && html.contains("id=\"cron\"")
            && html.contains("id=\"retry_delay\"")
    );
    let (_, _, html) = send(&state, Method::GET, &get("op=add_header"), true, "").await;
    assert!(html.contains("id=\"header-name-0\""));

    let (_, _, html) = send(
        &state,
        Method::GET,
        &format!(
            "http://{addr}/ui/jobs/new/preview?url=https%3A%2F%2Fexample.com&when=delay&delay=30"
        ),
        true,
        "",
    )
    .await;
    assert!(html.contains("POST /to/https://example.com"));
    assert!(html.contains("?_delay=30"));
    Ok(())
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn api_reports_every_invalid_parameter(pool: PgPool) -> anyhow::Result<()> {
    let state = common::state(pool.clone());
    let addr = common::spawn_app(&state).await;
    let long_id = "x".repeat(65);
    let response = common::create(
        &state,
        addr,
        &format!("http://{addr}/echo?_delay=soon&_timeout=0&_interval=0&_id={long_id}"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let bytes = response.into_body().collect().await?.to_bytes();
    let problem: serde_json::Value = serde_json::from_slice(&bytes)?;
    let params: Vec<&str> = problem["errors"]
        .as_array()
        .expect("errors")
        .iter()
        .filter_map(|e| e["param"].as_str())
        .collect();
    assert_eq!(params, ["_delay", "_timeout", "_interval", "_id"]);
    assert_eq!(common::count(&pool, "SELECT count(*) FROM jobs").await, 0);
    Ok(())
}
