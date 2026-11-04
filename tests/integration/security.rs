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

const TOKEN: &str = "0123456789abcdef0123456789abcdef";
const NEXT_TOKEN: &str = "fedcba9876543210fedcba9876543210";

#[sqlx::test(migrations = "./sql/migrations")]
async fn api_token_opens_the_api(pool: PgPool) -> anyhow::Result<()> {
    let tokens = format!("{TOKEN},{NEXT_TOKEN}");
    let state = common::state_with(pool.clone(), &["--api-token", &tokens]);
    let addr = common::spawn_app(&state).await;
    let url = |path: &str| format!("http://{addr}{path}");
    let create = url(&format!("/to/http://{addr}/echo?_delay=600"));

    // no token, a wrong one, another scheme
    for (headers, challenge) in [
        (vec![], "Bearer realm=\"irisqo\""),
        (
            vec![("authorization", "Bearer 0123456789abcdef0123456789abcdeX")],
            "Bearer realm=\"irisqo\", error=\"invalid_token\"",
        ),
        (
            vec![("authorization", "Basic dXNlcjpwYXNz")],
            "Bearer realm=\"irisqo\"",
        ),
    ] {
        let response = common::request_with(&state, Method::POST, &create, &headers).await;
        assert_eq!(
            common::header(&response, "www-authenticate").as_deref(),
            Some(challenge)
        );
        let (status, content_type, body) = problem(response).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{headers:?}");
        assert_eq!(content_type, "application/problem+json");
        assert_eq!(body["title"], "Unauthorized");
    }
    let response = common::request(&state, Method::GET, &url("/api/v1/schedules")).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "/api/v1 too");
    assert_eq!(common::count(&pool, "SELECT count(*) FROM jobs").await, 0);

    // either token; the caller's Authorization is not forwarded, Irisqo-Forward-* is
    let bearer = format!("Bearer {TOKEN}");
    let response = common::request_with(
        &state,
        Method::POST,
        &create,
        &[
            ("authorization", bearer.as_str()),
            ("irisqo-forward-x-api-key", "target-key"),
        ],
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let id = common::header(&response, "job-id").expect("job-id");
    let headers: serde_json::Value = sqlx::query_scalar("SELECT headers FROM jobs")
        .fetch_one(&pool)
        .await?;
    assert_eq!(headers, serde_json::json!({ "x-api-key": "target-key" }));

    let next = format!("bearer {NEXT_TOKEN}");
    let response = common::request_with(
        &state,
        Method::GET,
        &url(&format!("/api/v1/jobs/{id}")),
        &[("authorization", next.as_str())],
    )
    .await;
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "the second token, any case of the scheme"
    );

    // the rest needs no token; an unknown path stays a 404
    for (path, expected) in [
        ("/live", StatusCode::OK),
        ("/ready", StatusCode::OK),
        ("/", StatusCode::OK),
        ("/ui/assets/app.css", StatusCode::OK),
        ("/api/v1/nope", StatusCode::NOT_FOUND),
    ] {
        let response = common::request(&state, Method::GET, &url(path)).await;
        assert_eq!(response.status(), expected, "{path}");
    }
    Ok(())
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn no_token_keeps_the_api_open(pool: PgPool) -> anyhow::Result<()> {
    let state = common::state(pool);
    let addr = common::spawn_app(&state).await;
    let response = common::create(&state, addr, &format!("http://{addr}/echo?_delay=600")).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    Ok(())
}

/// A request with `headers` and a form `body`.
async fn send_form(
    state: &irisqo::AppState,
    uri: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> hyper::Response<hyper::body::Incoming> {
    let mut request = hyper::Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header("content-type", "application/x-www-form-urlencoded");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let request = request
        .body(http_body_util::Full::new(bytes::Bytes::from(
            body.to_owned(),
        )))
        .expect("request");
    state.client.request(request).await.expect("response")
}

async fn text(response: hyper::Response<hyper::body::Incoming>) -> String {
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    String::from_utf8_lossy(&bytes).into_owned()
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn ui_asks_for_the_token(pool: PgPool) -> anyhow::Result<()> {
    let tokens = format!("{TOKEN},{NEXT_TOKEN}");
    let state = common::state_with(pool.clone(), &["--api-token", &tokens]);
    let addr = common::spawn_app(&state).await;
    let url = |path: &str| format!("http://{addr}{path}");

    // no pass: a page goes to the login, back to where it was; htmx gets HX-Redirect
    let response = common::request_with(&state, Method::GET, &url("/ui/jobs"), &[]).await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        common::header(&response, "location").as_deref(),
        Some("/ui/login")
    );
    let response = common::request_with(&state, Method::GET, &url("/ui/jobs/5?tab=1"), &[]).await;
    assert_eq!(
        common::header(&response, "location").as_deref(),
        Some("/ui/login?next=%2Fui%2Fjobs%2F5%3Ftab%3D1")
    );
    let current = url("/ui/schedules");
    let response = common::request_with(
        &state,
        Method::GET,
        &url("/ui/instances/table"),
        &[("hx-request", "true"), ("hx-current-url", current.as_str())],
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        common::header(&response, "hx-redirect").as_deref(),
        Some("/ui/login?next=%2Fui%2Fschedules")
    );
    let response = send_form(&state, &url("/ui/jobs"), &[], "url=https://example.com").await;
    assert_eq!(
        response.status(),
        StatusCode::SEE_OTHER,
        "no job without a pass"
    );
    assert_eq!(common::count(&pool, "SELECT count(*) FROM jobs").await, 0);
    for open in ["/ui/login", "/ui/assets/app.css", "/favicon.ico"] {
        let response = common::request_with(&state, Method::GET, &url(open), &[]).await;
        assert_eq!(response.status(), StatusCode::OK, "{open}");
    }

    // a wrong token, then a good one; next cannot lead out of the UI
    let response = send_form(
        &state,
        &url("/ui/login"),
        &[],
        "token=nope&next=%2Fui%2Fschedules",
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let page = text(response).await;
    assert!(page.contains("The token is not valid.") && page.contains("value=\"/ui/schedules\""));
    let response = send_form(
        &state,
        &url("/ui/login"),
        &[],
        &format!("token={TOKEN}&next=%2Fui%2Fschedules"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        common::header(&response, "location").as_deref(),
        Some("/ui/schedules")
    );
    let set_cookie = common::header(&response, "set-cookie").expect("set-cookie");
    assert!(set_cookie.starts_with("irisqo_ui="), "{set_cookie}");
    for attribute in ["Path=/ui", "Max-Age=604800", "HttpOnly", "SameSite=Lax"] {
        assert!(set_cookie.contains(attribute), "{attribute}: {set_cookie}");
    }
    assert!(!set_cookie.contains("Secure"), "plain HTTP: {set_cookie}");
    assert!(!set_cookie.contains(TOKEN), "a pass, not the token");
    let pass = set_cookie
        .split(';')
        .next()
        .and_then(|pair| pair.split_once('='))
        .map(|(_, value)| value.to_owned())
        .expect("pass");
    let cookie = format!("irisqo_ui={pass}");

    let response = send_form(
        &state,
        &url("/ui/login"),
        &[("x-forwarded-proto", "https")],
        &format!("token={NEXT_TOKEN}&next=https%3A%2F%2Fevil.example%2F"),
    )
    .await;
    assert_eq!(
        common::header(&response, "location").as_deref(),
        Some("/ui/jobs")
    );
    assert!(common::header(&response, "set-cookie").is_some_and(|c| c.contains("; Secure")));

    // with the pass: the UI, and the logout
    let response = common::request_with(
        &state,
        Method::GET,
        &url("/ui/jobs"),
        &[("cookie", cookie.as_str())],
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(text(response).await.contains("action=\"/ui/logout\""));
    let response = send_form(
        &state,
        &url("/ui/logout"),
        &[("cookie", cookie.as_str())],
        "",
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert!(
        common::header(&response, "set-cookie")
            .is_some_and(|c| c.contains("irisqo_ui=;") && c.contains("Max-Age=0"))
    );

    // a pass of a token no longer listed, an expired one, a forged one
    let other = common::state_with(pool.clone(), &["--api-token", NEXT_TOKEN]);
    let other_addr = common::spawn_app(&other).await;
    let (expires, tag) = pass.split_once('.').expect("expires.tag");
    for (instance, candidate) in [
        (other_addr, cookie.clone()),
        (addr, format!("irisqo_ui=1.{tag}")),
        (addr, format!("irisqo_ui={expires}.{}", "0".repeat(64))),
    ] {
        let response = common::request_with(
            &state,
            Method::GET,
            &format!("http://{instance}/ui/jobs"),
            &[("cookie", candidate.as_str())],
        )
        .await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER, "{candidate}");
    }
    Ok(())
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn ui_without_token_is_open(pool: PgPool) -> anyhow::Result<()> {
    let state = common::state(pool);
    let addr = common::spawn_app(&state).await;
    let response =
        common::request_with(&state, Method::GET, &format!("http://{addr}/ui/jobs"), &[]).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(!text(response).await.contains("/ui/logout"));
    let response = common::request_with(
        &state,
        Method::GET,
        &format!("http://{addr}/ui/login?next=%2Fui%2Fschedules"),
        &[],
    )
    .await;
    assert_eq!(
        common::header(&response, "location").as_deref(),
        Some("/ui/schedules")
    );
    Ok(())
}
