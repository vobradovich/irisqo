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
async fn a_flow_is_declared_changed_paused_and_deleted(pool: PgPool) -> anyhow::Result<()> {
    let state = common::state(pool.clone());
    let addr = common::spawn_app(&state).await;
    let url = format!("http://{addr}/api/v1/flows");

    let (status, row) = json(
        &state,
        Method::PUT,
        &format!("{url}/emails"),
        r#"{"concurrency": 3, "rate": 5, "rate_period": 60}"#,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{row}");
    assert_eq!(row["flow_id"], "emails");
    assert_eq!(row["concurrency"], 3);
    assert_eq!(row["rate"], 5);
    assert_eq!(row["rate_period"], 60);
    assert!(row.get("ordered").is_none(), "{row}");
    assert_eq!(row["paused"], false);

    let (status, body) = json(
        &state,
        Method::PUT,
        &format!("{url}/Bad"),
        r#"{"concurrency": 0}"#,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["errors"].as_array().map(Vec::len), Some(2), "{body}");

    // the settings are replaced as a whole: the rate goes
    let (status, row) = json(
        &state,
        Method::PUT,
        &format!("{url}/emails"),
        r#"{"concurrency": 2}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{row}");
    assert_eq!(row["concurrency"], 2);
    assert!(row["rate"].is_null());
    // no body is all the defaults; a new name is created
    let (status, row) = json(&state, Method::PUT, &format!("{url}/nope"), "").await;
    assert_eq!(status, StatusCode::CREATED, "{row}");
    assert_eq!(row["concurrency"], 1);
    let gone = common::request(&state, Method::DELETE, &format!("{url}/nope")).await;
    assert_eq!(gone.status(), StatusCode::NO_CONTENT);

    let paused = |at: &'static str| {
        let (state, url) = (state.clone(), format!("{url}/emails{at}"));
        async move { common::request(&state, Method::POST, &url).await.status() }
    };
    assert_eq!(paused("/pause").await, StatusCode::NO_CONTENT);
    let (_, row) = json(&state, Method::GET, &format!("{url}/emails"), "").await;
    assert_eq!(row["paused"], true);
    // a paused flow holds its jobs
    let id = common::insert_enqueued(&pool, 1).await[0];
    sqlx::query("UPDATE enqueued SET flow_key = 'emails' WHERE id = $1")
        .bind(id)
        .execute(&pool)
        .await?;
    assert!(common::fetch(&state, 10).await.is_empty());
    // the list counts the waiting job, next to the flow's own fields
    let (_, list) = json(&state, Method::GET, &url, "").await;
    let first = &list["data"][0];
    assert_eq!(first["flow_id"], "emails", "{list}");
    assert_eq!(
        (&first["waiting"], &first["processing"]),
        (&1.into(), &0.into()),
        "{list}"
    );
    assert_eq!(paused("/resume").await, StatusCode::NO_CONTENT);
    assert_eq!(common::fetch(&state, 10).await.len(), 1);

    let deleted = common::request(&state, Method::DELETE, &format!("{url}/emails")).await;
    assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
    let deleted = common::request(&state, Method::DELETE, &format!("{url}/emails")).await;
    assert_eq!(deleted.status(), StatusCode::NOT_FOUND);
    // the jobs use the name: it is no longer declared, but still counted
    let (_, list) = json(&state, Method::GET, &url, "").await;
    assert!(list["data"].as_array().is_some_and(Vec::is_empty), "{list}");
    let other = format!("http://{addr}/api/v1/undeclared-flows");
    let (_, list) = json(&state, Method::GET, &other, "").await;
    assert_eq!(list["data"][0]["flow_id"], "emails", "{list}");
    assert_eq!(list["data"][0]["processing"], 1, "{list}");
    let (status, _) = json(&state, Method::GET, &format!("{url}/emails"), "").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    Ok(())
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn the_ui_manages_flows(pool: PgPool) -> anyhow::Result<()> {
    let state = common::state(pool.clone());
    let addr = common::spawn_app(&state).await;
    let base = format!("http://{addr}/ui/flows");

    let (status, _, html) = send(&state, Method::GET, &base, false, "").await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("No declared flows") && html.contains(r#"aria-current="page">Flows"#));

    // a wrong form is shown again with the reasons
    let (status, _, html) = send(
        &state,
        Method::POST,
        &base,
        false,
        "flow_key=reports&concurrency=x",
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(html.contains("must be a whole number") && html.contains(r#"value="reports""#));

    let (status, _, _) = send(
        &state,
        Method::POST,
        &base,
        false,
        "flow_key=reports&concurrency=4&rate=10&rate_period=60",
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
        "concurrency=2",
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let concurrency: i32 =
        sqlx::query_scalar("SELECT concurrency FROM flows WHERE flow_id = 'reports'")
            .fetch_one(&pool)
            .await?;
    assert_eq!(concurrency, 2);

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
    assert!(html.contains("No declared flows"));
    Ok(())
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn the_ui_filters_and_pages_the_flows(pool: PgPool) -> anyhow::Result<()> {
    let state = common::state(pool.clone());
    let addr = common::spawn_app(&state).await;
    let base = format!("http://{addr}/ui/flows");
    // 52 declared flows, and jobs in two names that nobody declared
    sqlx::query("INSERT INTO flows(flow_id) SELECT 'q' || lpad(g::text, 2, '0') FROM generate_series(1, 52) g")
        .execute(&pool)
        .await?;
    for (id, flow) in common::insert_enqueued(&pool, 2)
        .await
        .into_iter()
        .zip(["free-a", "free-b"])
    {
        sqlx::query("UPDATE enqueued SET flow_key = $2 WHERE id = $1")
            .bind(id)
            .bind(flow)
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

#[sqlx::test(migrations = "./sql/migrations")]
async fn the_jobs_of_a_flow_are_held_and_are_no_lag(pool: PgPool) -> anyhow::Result<()> {
    let ids = common::insert_enqueued(&pool, 3).await;
    // two jobs of a flow, one of them an hour old; one job without a flow
    sqlx::query("UPDATE enqueued SET flow_key = 'q' WHERE id = ANY($1)")
        .bind(&ids[..2])
        .execute(&pool)
        .await?;
    sqlx::query("UPDATE enqueued SET at = now() - interval '1 hour' WHERE id = $1")
        .bind(ids[0])
        .execute(&pool)
        .await?;
    let stats = irisqo::db::stats::queue_stats(&pool).await?;
    assert_eq!((stats.enqueued, stats.held), (3, 2));
    assert!(stats.lag < 60.0, "a held job is no lag: {}", stats.lag);

    // the job without a flow waits: that is the lag
    sqlx::query("UPDATE enqueued SET at = now() - interval '1 hour' WHERE id = $1")
        .bind(ids[2])
        .execute(&pool)
        .await?;
    let stats = irisqo::db::stats::queue_stats(&pool).await?;
    assert!(stats.lag > 3000.0, "{}", stats.lag);
    Ok(())
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn the_ui_shows_until_when_the_rate_limit_holds_the_jobs(pool: PgPool) -> anyhow::Result<()> {
    let state = common::state(pool.clone());
    let addr = common::spawn_app(&state).await;
    let base = format!("http://{addr}/ui/flows");
    sqlx::query(
        "INSERT INTO flows(flow_id, concurrency, rate, rate_period) VALUES ('q', 8, 1, 3600)",
    )
    .execute(&pool)
    .await?;
    for id in common::insert_enqueued(&pool, 2).await {
        sqlx::query("UPDATE enqueued SET flow_key = 'q' WHERE id = $1")
            .bind(id)
            .execute(&pool)
            .await?;
    }
    // nothing started yet: two jobs wait, no limit holds them
    let (_, _, html) = send(&state, Method::GET, &base, false, "").await;
    assert!(
        !html.contains("Rate limited") && html.contains("1 per 1 h · 1 left"),
        "{html}"
    );

    // one start per hour: the second job waits until the bucket has a start free
    assert_eq!(common::fetch(&state, 10).await.len(), 1);
    let (_, _, html) = send(&state, Method::GET, &base, false, "").await;
    assert!(
        html.contains("Rate limited until") && html.contains("1 per 1 h · 0 left"),
        "{html}"
    );
    Ok(())
}
