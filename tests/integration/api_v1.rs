//! `POST /api/v1/jobs` and `PUT /api/v1/schedules/{id}`: a job described in JSON (job format v1).
//! What the job is made of and which values are refused is checked without a database by
//! `handlers::jobs::tests` and `features::schedules::http::tests`.

use hyper::{Method, StatusCode};
use sqlx::PgPool;

use crate::common;

async fn post_job(
    state: &irisqo::AppState,
    addr: std::net::SocketAddr,
    json: &str,
) -> hyper::Response<hyper::body::Incoming> {
    request(state, addr, Method::POST, "jobs", json).await
}

async fn put_schedule(
    state: &irisqo::AppState,
    addr: std::net::SocketAddr,
    schedule_id: &str,
    json: &str,
) -> hyper::Response<hyper::body::Incoming> {
    request(
        state,
        addr,
        Method::PUT,
        &format!("schedules/{schedule_id}"),
        json,
    )
    .await
}

async fn request(
    state: &irisqo::AppState,
    addr: std::net::SocketAddr,
    method: Method,
    path: &str,
    json: &str,
) -> hyper::Response<hyper::body::Incoming> {
    let uri = format!("http://{addr}/api/v1/{path}");
    let headers = [("content-type", "application/json")];
    common::send(state, method, &uri, &headers, json).await
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn json_job_is_queued_and_delivered(pool: PgPool) -> anyhow::Result<()> {
    let state = common::state(pool.clone());
    let addr = common::spawn_app(&state).await;

    let json = format!(
        r#"{{"sink":"http","method":"PUT","url":"http://{addr}/echo",
              "retry":"fixed","retry_count":2,"retry_delay":1,"timeout":5000,
            "headers":{{"x-a":"b"}},"body":{{"n":1}},"key":"v1-job"}}"#
    );
    let response = post_job(&state, addr, &json).await;
    assert_eq!(response.status(), StatusCode::OK);
    let job_id: i64 = common::header(&response, "irisqo-job-id")
        .and_then(|v| v.parse().ok())
        .expect("job id");
    assert_eq!(
        common::header(&response, "location"),
        Some(format!("/api/v1/jobs/{job_id}"))
    );

    // a repeated key returns the same job
    let again = post_job(&state, addr, &json).await;
    assert_eq!(
        common::header(&again, "irisqo-job-id"),
        Some(job_id.to_string())
    );
    assert_eq!(
        common::text(again).await,
        format!(r#"{{"id":{job_id},"key":"v1-job"}}"#)
    );

    assert_eq!(common::run_enqueued(&state).await, 1);
    assert_eq!(
        common::processed_status(&pool, job_id).await.as_deref(),
        Some("completed")
    );
    Ok(())
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn json_job_with_delay_is_scheduled(pool: PgPool) -> anyhow::Result<()> {
    let state = common::state(pool.clone());
    let addr = common::spawn_app(&state).await;

    let json =
        format!(r#"{{"sink":"http","method":"POST","url":"http://{addr}/echo","delay":3600}}"#);
    let response = post_job(&state, addr, &json).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        common::count(&pool, "SELECT count(*) FROM scheduled").await,
        1
    );
    Ok(())
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn an_invalid_job_is_a_problem_and_is_not_stored(pool: PgPool) -> anyhow::Result<()> {
    let state = common::state(pool.clone());
    let addr = common::spawn_app(&state).await;

    // every invalid field is reported
    let json = r#"{"sink":"http","method":"GET","url":"ftp://a.example/",
                   "headers":{"host":"x"},"key":"a\u0001b"}"#;
    let response = post_job(&state, addr, json).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = common::text(response).await;
    assert!(body.contains("\"parameter\":\"sink\""), "{body}");

    // not JSON, an unknown sink, no sink
    for bad in ["nope", r#"{"sink":"smtp"}"#, "{}"] {
        let response = post_job(&state, addr, bad).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{bad}");
    }
    assert_eq!(common::count(&pool, "SELECT count(*) FROM jobs").await, 0);
    Ok(())
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn a_schedule_is_declared_with_put(pool: PgPool) -> anyhow::Result<()> {
    let state = common::state(pool.clone());
    let addr = common::spawn_app(&state).await;
    let job = |interval: u32, path: &str| {
        format!(
            r#"{{"sink":"http","method":"POST","url":"http://{addr}/{path}","interval":{interval}}}"#
        )
    };

    // created with its first run
    let response = put_schedule(&state, addr, "nightly", &job(3600, "echo")).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        common::header(&response, "location").as_deref(),
        Some("/api/v1/schedules/nightly")
    );
    assert_eq!(
        common::header(&response, "irisqo-schedule-id").as_deref(),
        Some("nightly")
    );
    let first = common::header(&response, "irisqo-job-id").expect("job id");
    assert_eq!(
        common::count(&pool, "SELECT count(*) FROM schedules").await,
        1
    );
    assert_eq!(
        common::count(&pool, "SELECT count(*) FROM scheduled").await,
        1
    );

    // a declaration replaces the waiting run (cancelled, as by a pause) with a new job: the new
    // job and rule, one run waiting
    let changed = put_schedule(&state, addr, "nightly", &job(7200, "error")).await;
    let second = common::header(&changed, "irisqo-job-id").expect("job id");
    assert_ne!(second, first);
    assert_eq!(
        common::count(&pool, "SELECT count(*) FROM schedules").await,
        1
    );
    assert_eq!(
        common::count(&pool, "SELECT count(*) FROM scheduled").await,
        1
    );
    assert_eq!(
        common::count(
            &pool,
            "SELECT count(*) FROM processed WHERE status = 'cancelled'"
        )
        .await,
        1
    );
    let (rule, url): (String, String) = sqlx::query_as(
        "SELECT s.rule, j.meta->>'url' FROM schedules s JOIN jobs j ON j.id = s.next_id",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(rule, "7200");
    assert!(url.ends_with("/error"), "{url}");

    // the same declaration again, in the same slot: the key of the run does not clash
    let again = put_schedule(&state, addr, "nightly", &job(7200, "error")).await;
    assert_eq!(again.status(), StatusCode::OK);
    assert_eq!(
        common::count(&pool, "SELECT count(*) FROM scheduled").await,
        1
    );

    // a paused schedule is resumed by its declaration
    sqlx::query("UPDATE schedules SET paused = true")
        .execute(&pool)
        .await?;
    put_schedule(&state, addr, "nightly", &job(7200, "error")).await;
    assert_eq!(
        common::count(&pool, "SELECT count(*) FROM schedules WHERE NOT paused").await,
        1
    );

    // not a schedule (no cron or interval), not an id
    let no_rule = format!(r#"{{"sink":"http","method":"POST","url":"http://{addr}/echo"}}"#);
    assert_eq!(
        put_schedule(&state, addr, "nightly", &no_rule)
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    let response = put_schedule(&state, addr, "a%01b", &job(60, "echo")).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    Ok(())
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn a_cron_schedule_runs_in_its_time_zone(pool: PgPool) -> anyhow::Result<()> {
    let state = common::state(pool.clone());
    let addr = common::spawn_app(&state).await;
    let declare = |tz: &'static str| {
        let state = state.clone();
        async move {
            let json = format!(
                r#"{{"sink":"http","method":"POST","url":"http://{addr}/echo",
                    "cron":"0 0 9 * * *","tz":"{tz}"}}"#
            );
            put_schedule(&state, addr, "morning", &json).await
        }
    };
    // the hour of the next run in UTC
    let next = || async {
        sqlx::query_as::<_, (String, i32)>(
            "SELECT tz, extract(hour FROM next_at AT TIME ZONE 'UTC')::int FROM schedules",
        )
        .fetch_one(&pool)
        .await
    };

    // 9:00 in Tokyo is 0:00 UTC
    assert_eq!(declare("Asia/Tokyo").await.status(), StatusCode::OK);
    assert_eq!(next().await?, ("Asia/Tokyo".to_owned(), 0));

    // another zone is another declaration: the waiting run is replaced
    assert_eq!(declare("UTC").await.status(), StatusCode::OK);
    assert_eq!(next().await?, ("UTC".to_owned(), 9));
    assert_eq!(
        common::count(&pool, "SELECT count(*) FROM scheduled").await,
        1
    );
    Ok(())
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn a_schedule_through_to_is_declared_the_same_way(pool: PgPool) -> anyhow::Result<()> {
    let state = common::state(pool.clone());
    let addr = common::spawn_app(&state).await;
    let target = |interval: u32| format!("http://{addr}/echo?_interval={interval}&_key=nightly");

    let first = common::create(&state, addr, &target(3600)).await;
    assert_eq!(first.status(), StatusCode::CREATED);
    assert_eq!(
        common::header(&first, "irisqo-schedule-id").as_deref(),
        Some("nightly")
    );

    // the same key with another rule changes the schedule: one schedule, one waiting run
    let second = common::create(&state, addr, &target(7200)).await;
    assert_ne!(
        common::header(&second, "irisqo-job-id"),
        common::header(&first, "irisqo-job-id")
    );
    assert_eq!(
        common::count(&pool, "SELECT count(*) FROM schedules").await,
        1
    );
    assert_eq!(
        common::count(&pool, "SELECT count(*) FROM scheduled").await,
        1
    );
    assert_eq!(
        common::count(&pool, "SELECT count(*) FROM schedules WHERE rule = '7200'").await,
        1
    );

    // no key: a new schedule with its own id
    let anonymous = common::create(&state, addr, &format!("http://{addr}/echo?_interval=60")).await;
    let id = common::header(&anonymous, "irisqo-schedule-id").expect("schedule id");
    assert_eq!(id.len(), 26, "{id}");
    assert_eq!(
        common::count(&pool, "SELECT count(*) FROM schedules").await,
        2
    );
    Ok(())
}

/// A run that was cancelled gives its key back: the schedule may need a run of that very time
/// (`{schedule_id}-{secs}`) again, and it must not clash with the cancelled one.
#[sqlx::test(migrations = "./sql/migrations")]
async fn a_cancelled_run_gives_its_key_back(pool: PgPool) -> anyhow::Result<()> {
    let state = common::state(pool.clone());
    let addr = common::spawn_app(&state).await;
    let json =
        format!(r#"{{"sink":"http","method":"POST","url":"http://{addr}/echo","interval":3600}}"#);
    let response = put_schedule(&state, addr, "nightly", &json).await;
    let run: i64 = common::header(&response, "irisqo-job-id")
        .and_then(|id| id.parse().ok())
        .expect("job id");
    let at: chrono::DateTime<chrono::Utc> = sqlx::query_scalar("SELECT at FROM scheduled")
        .fetch_one(&pool)
        .await?;

    let cancel = common::request(
        &state,
        Method::POST,
        &format!("http://{addr}/api/v1/jobs/{run}/cancel"),
    )
    .await;
    assert_eq!(cancel.status(), StatusCode::NO_CONTENT);
    let keys = common::count(&pool, "SELECT count(*) FROM jobs WHERE key IS NULL").await;
    assert_eq!(keys, 1, "the cancelled run gave its key back");

    // the run that continues the schedule is at the time of the cancelled one
    let next = irisqo::features::schedules::clone_schedule_at(&pool, run, at, "test").await?;
    assert!(next.is_some());
    let key: String = sqlx::query_scalar("SELECT key FROM jobs WHERE id = $1")
        .bind(next)
        .fetch_one(&pool)
        .await?;
    assert_eq!(key, format!("nightly-{}", at.timestamp()));
    Ok(())
}

/// A run that finishes while a declaration is in flight must not continue the old schedule.
#[sqlx::test(migrations = "./sql/migrations")]
async fn a_run_that_finishes_during_a_declaration_does_not_continue_the_old_schedule(
    pool: PgPool,
) -> anyhow::Result<()> {
    let state = common::state(pool.clone());
    let addr = common::spawn_app(&state).await;
    let json =
        format!(r#"{{"sink":"http","method":"POST","url":"http://{addr}/echo","interval":3600}}"#);
    let response = put_schedule(&state, addr, "nightly", &json).await;
    let old_run: i64 = common::header(&response, "irisqo-job-id")
        .and_then(|id| id.parse().ok())
        .expect("job id");

    // a declaration in flight: the new run is the next one, and it is not committed yet
    let new_run: i64 = sqlx::query_scalar(
        "INSERT INTO jobs(meta, schedule_id) VALUES ('{\"sink\": \"noop\", \"retry\": \"none\"}', 'nightly') RETURNING id",
    )
    .fetch_one(&pool)
    .await?;
    let mut declaration = pool.begin().await?;
    sqlx::query("UPDATE schedules SET next_id = $1")
        .bind(new_run)
        .execute(&mut *declaration)
        .await?;

    // the old run finishes: it waits for the row of the schedule
    let worker = tokio::spawn({
        let pool = pool.clone();
        async move {
            irisqo::features::schedules::clone_schedule_at(
                &pool,
                old_run,
                chrono::Utc::now(),
                "test",
            )
            .await
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    declaration.commit().await?;

    assert_eq!(
        worker.await??,
        None,
        "the old run is not the next one any more"
    );
    let next_id: Option<i64> = sqlx::query_scalar("SELECT next_id FROM schedules")
        .fetch_one(&pool)
        .await?;
    assert_eq!(next_id, Some(new_run));
    Ok(())
}
