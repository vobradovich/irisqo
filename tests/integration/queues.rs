use hyper::{Method, StatusCode};
use sqlx::PgPool;

use crate::{common, ui::send};

async fn json(
    state: &irisqo::AppState,
    method: Method,
    uri: &str,
    body: &str,
) -> (StatusCode, serde_json::Value) {
    let response = common::send(
        state,
        method,
        uri,
        &[("content-type", "application/json")],
        body,
    )
    .await;
    let status = response.status();
    let text = common::text(response).await;
    (status, serde_json::from_str(&text).unwrap_or_default())
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn a_queue_is_declared_changed_paused_and_deleted(pool: PgPool) -> anyhow::Result<()> {
    let state = common::state(pool.clone());
    let addr = common::spawn_app(&state).await;
    let url = format!("http://{addr}/api/v1/queues");

    let (status, row) = json(
        &state,
        Method::PUT,
        &format!("{url}/emails"),
        r#"{"parallelism": 3, "rate": 5, "rate_period": 60}"#,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{row}");
    assert_eq!(row["queue_id"], "emails");
    assert_eq!(row["parallelism"], 3);
    assert_eq!(row["rate"], 5);
    assert_eq!(row["rate_period"], 60);
    assert_eq!(row["ordered"], true);
    assert_eq!(row["paused"], false);

    let (status, body) = json(
        &state,
        Method::PUT,
        &format!("{url}/Bad"),
        r#"{"parallelism": 0}"#,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["errors"].as_array().map(Vec::len), Some(2), "{body}");

    // the settings are replaced as a whole: the rate goes, the order is off
    let (status, row) = json(
        &state,
        Method::PUT,
        &format!("{url}/emails"),
        r#"{"parallelism": 2, "ordered": false}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{row}");
    assert_eq!(row["parallelism"], 2);
    assert_eq!(row["ordered"], false);
    assert!(row["rate"].is_null());
    // no body is all the defaults; a new name is created
    let (status, row) = json(&state, Method::PUT, &format!("{url}/nope"), "").await;
    assert_eq!(status, StatusCode::CREATED, "{row}");
    assert_eq!(row["parallelism"], 1);
    let gone = common::request(&state, Method::DELETE, &format!("{url}/nope")).await;
    assert_eq!(gone.status(), StatusCode::NO_CONTENT);

    let paused = |at: &'static str| {
        let (state, url) = (state.clone(), format!("{url}/emails{at}"));
        async move { common::request(&state, Method::POST, &url).await.status() }
    };
    assert_eq!(paused("/pause").await, StatusCode::NO_CONTENT);
    let (_, row) = json(&state, Method::GET, &format!("{url}/emails"), "").await;
    assert_eq!(row["paused"], true);
    // a paused queue holds its jobs
    let id = common::insert_enqueued(&pool, 1).await[0];
    sqlx::query("UPDATE enqueued SET queue = 'emails' WHERE id = $1")
        .bind(id)
        .execute(&pool)
        .await?;
    assert!(common::fetch(&state, 10).await.is_empty());
    assert_eq!(paused("/resume").await, StatusCode::NO_CONTENT);
    assert_eq!(common::fetch(&state, 10).await.len(), 1);

    let deleted = common::request(&state, Method::DELETE, &format!("{url}/emails")).await;
    assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
    let deleted = common::request(&state, Method::DELETE, &format!("{url}/emails")).await;
    assert_eq!(deleted.status(), StatusCode::NOT_FOUND);
    // the jobs use the name: it is no longer declared, but still counted
    let (_, list) = json(&state, Method::GET, &url, "").await;
    assert!(list["data"].as_array().is_some_and(Vec::is_empty), "{list}");
    let other = format!("http://{addr}/api/v1/undeclared-queues");
    let (_, list) = json(&state, Method::GET, &other, "").await;
    assert_eq!(list["data"][0]["queue_id"], "emails", "{list}");
    assert_eq!(list["data"][0]["processing"], 1, "{list}");
    let (status, _) = json(&state, Method::GET, &format!("{url}/emails"), "").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    Ok(())
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn the_ui_manages_queues(pool: PgPool) -> anyhow::Result<()> {
    let state = common::state(pool.clone());
    let addr = common::spawn_app(&state).await;
    let base = format!("http://{addr}/ui/queues");

    let (status, _, html) = send(&state, Method::GET, &base, false, "").await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("No declared queues") && html.contains(r#"aria-current="page">Queues"#));

    // a wrong form is shown again with the reasons
    let (status, _, html) = send(
        &state,
        Method::POST,
        &base,
        false,
        "queue=reports&parallelism=x&ordered=on",
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(html.contains("must be a whole number") && html.contains(r#"value="reports""#));

    let (status, _, _) = send(
        &state,
        Method::POST,
        &base,
        false,
        "queue=reports&parallelism=4&ordered=on&rate=10&rate_period=60",
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let (_, _, html) = send(&state, Method::GET, &base, false, "").await;
    assert!(
        html.contains("reports") && html.contains("10 per 1 min"),
        "{html}"
    );

    let (status, _, html) = send(
        &state,
        Method::GET,
        &format!("{base}/reports/edit"),
        false,
        "",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains(r#"value="4""#));
    let (status, _, _) = send(
        &state,
        Method::POST,
        &format!("{base}/reports"),
        false,
        "parallelism=2",
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let (parallelism, ordered): (i32, bool) =
        sqlx::query_as("SELECT parallelism, ordered FROM queues WHERE queue_id = 'reports'")
            .fetch_one(&pool)
            .await?;
    assert_eq!((parallelism, ordered), (2, false));

    let (_, _, html) = send(
        &state,
        Method::POST,
        &format!("{base}/reports/pause"),
        true,
        "",
    )
    .await;
    assert!(html.contains("Paused") && html.contains("Resume"));
    let (_, _, html) = send(
        &state,
        Method::POST,
        &format!("{base}/reports/resume"),
        true,
        "",
    )
    .await;
    assert!(html.contains("Active") && html.contains("Pause"));
    let (_, _, html) = send(
        &state,
        Method::POST,
        &format!("{base}/reports/delete"),
        true,
        "",
    )
    .await;
    assert!(html.contains("No declared queues"));
    Ok(())
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn the_ui_filters_and_pages_the_queues(pool: PgPool) -> anyhow::Result<()> {
    let state = common::state(pool.clone());
    let addr = common::spawn_app(&state).await;
    let base = format!("http://{addr}/ui/queues");
    // 52 declared queues, and jobs in two names that nobody declared
    sqlx::query("INSERT INTO queues(queue_id) SELECT 'q' || lpad(g::text, 2, '0') FROM generate_series(1, 52) g")
        .execute(&pool)
        .await?;
    for (id, queue) in common::insert_enqueued(&pool, 2)
        .await
        .into_iter()
        .zip(["free-a", "free-b"])
    {
        sqlx::query("UPDATE enqueued SET queue = $2 WHERE id = $1")
            .bind(id)
            .bind(queue)
            .execute(&pool)
            .await?;
    }

    // declared by default: a page of 50 by name, no others
    let (_, _, html) = send(&state, Method::GET, &base, false, "").await;
    assert!(html.contains("q01") && html.contains("q50") && !html.contains("q51"));
    assert!(!html.contains("free-a") && html.contains("Next"));
    let (_, _, html) = send(
        &state,
        Method::GET,
        &format!("{base}?part=declared&after=q50"),
        false,
        "",
    )
    .await;
    assert!(html.contains("q51") && html.contains("q52") && !html.contains("free-a"));

    let (_, _, html) = send(
        &state,
        Method::GET,
        &format!("{base}?show=undeclared"),
        false,
        "",
    )
    .await;
    assert!(html.contains("free-a") && html.contains("free-b") && !html.contains("q01"));

    // all: the declared first, then the others, on the page that has room
    let (_, _, html) = send(
        &state,
        Method::GET,
        &format!("{base}?show=all&part=declared&after=q50"),
        false,
        "",
    )
    .await;
    assert!(html.contains("q52") && html.contains("free-a") && html.contains("free-b"));
    assert!(html.find("q52") < html.find("free-a"));
    let (_, _, html) = send(&state, Method::GET, &format!("{base}?show=all"), false, "").await;
    assert!(!html.contains("free-a") && html.contains("Next"));
    Ok(())
}
