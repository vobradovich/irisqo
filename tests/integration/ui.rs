use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Method, StatusCode};
use sqlx::PgPool;

use crate::common;

pub(crate) async fn send(
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
        Some(&b"/ui/jobs"[..])
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
        "/ui/assets/htmx-4.0.1.min.js",
        "/ui/assets/pico-2.1.1.classless.min.css",
        "/ui/assets/app.css",
        "/ui/assets/logo.svg",
        "/ui/assets/favicon-32.png",
        "/ui/assets/apple-touch-icon.png",
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
    let (status, headers, _) = send(
        &state,
        Method::GET,
        &format!("http://{addr}/favicon.ico"),
        false,
        "",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers.get("content-type").map(|v| v.as_bytes()),
        Some(&b"image/png"[..])
    );
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
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
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
async fn dependent_fields_keep_their_values(pool: PgPool) -> anyhow::Result<()> {
    let state = common::state(pool);
    let addr = common::spawn_app(&state).await;
    let get = |query: &str| format!("http://{addr}/ui/jobs/new/form?{query}");

    // every dependent field is rendered (CSS shows the picked ones), so a value typed
    // for another choice is still there; the preview reads only the picked choice
    let (_, _, html) = send(
        &state,
        Method::GET,
        &get("when=now&delay=30&repeat=once&cron=*/5+*+*+*+*&retry=none&retry_count=3"),
        true,
        "",
    )
    .await;
    for field in [
        "class=\"dep when-delay\"",
        "class=\"dep repeat-cron\"",
        "class=\"narrow dep retry-any\"",
        "id=\"delay\" name=\"delay\" type=\"number\" min=\"0\" value=\"30\"",
        "value=\"*/5 * * * *\"",
    ] {
        assert!(html.contains(field), "{field}");
    }
    assert!(
        !html.contains("hx-trigger=\"change\""),
        "choices make no request"
    );
    assert!(
        !html.contains("_delay=30"),
        "the preview ignores an unpicked choice"
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
    let problem = |target: String| {
        let state = state.clone();
        async move {
            let response = common::create(&state, addr, &target).await;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{target}");
            assert_eq!(
                common::header(&response, "content-type").as_deref(),
                Some("application/problem+json")
            );
            let bytes = response
                .into_body()
                .collect()
                .await
                .expect("body")
                .to_bytes();
            serde_json::from_slice::<serde_json::Value>(&bytes).expect("json")
        }
    };
    let long_id = "x".repeat(65);
    let body = problem(format!(
        "http://{addr}/echo?_delay=soon&_timeout=0&_interval=0&_id={long_id}"
    ))
    .await;
    // RFC 9457: a problem type of its own, a stable title, the errors as an extension
    assert_eq!(body["type"], irisqo::models::INVALID_PARAMS_TYPE);
    assert_eq!(body["title"], "Invalid parameters");
    assert_eq!(body["status"], 400);
    assert_eq!(body["detail"], "4 parameters are invalid.");
    let errors = body["errors"].as_array().expect("errors");
    let params: Vec<&str> = errors
        .iter()
        .filter_map(|e| e["parameter"].as_str())
        .collect();
    assert_eq!(params, ["_delay", "_timeout", "_interval", "_id"]);
    assert_eq!(errors[0]["detail"], "must be a number of seconds");

    // a schedule that would never run
    let until = chrono::Utc::now().timestamp() + 60;
    let body = problem(format!(
        "http://{addr}/echo?_cron=0+0+0+1+1+*&_until={until}"
    ))
    .await;
    assert_eq!(body["errors"][0]["parameter"], "_until");
    let body = problem("not a url".replace(' ', "%20")).await;
    assert_eq!(body["errors"][0]["parameter"], "url");
    assert_eq!(common::count(&pool, "SELECT count(*) FROM jobs").await, 0);
    Ok(())
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn server_errors_do_not_leak_internals(pool: PgPool) -> anyhow::Result<()> {
    let state = common::state(pool.clone());
    let addr = common::spawn_app(&state).await;
    // break the database under the API
    sqlx::query("DROP TABLE history CASCADE")
        .execute(&pool)
        .await?;
    let response = common::create(&state, addr, &format!("http://{addr}/echo")).await;
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let bytes = response.into_body().collect().await?.to_bytes();
    let body: serde_json::Value = serde_json::from_slice(&bytes)?;
    assert_eq!(body["title"], "Internal Server Error");
    assert!(body.get("instance").is_none());
    let text = String::from_utf8_lossy(&bytes);
    assert!(
        !text.contains("history") && !text.contains("relation"),
        "{text}"
    );
    Ok(())
}
