use hyper::{Method, StatusCode};
use sqlx::PgPool;

use crate::common;

#[sqlx::test(migrations = "./sql/migrations")]
async fn invalid_schedule_params_are_rejected(pool: PgPool) -> anyhow::Result<()> {
    let state = common::state(pool.clone());
    let addr = common::spawn_app(&state).await;
    let target = format!("http://{addr}/echo");

    for query in [
        "_interval=0",
        "_delay_until=99999999999999",
        "_interval=60&_until=99999999999999",
    ] {
        let response = common::create(&state, addr, &format!("{target}?{query}")).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{query}");
    }
    assert_eq!(common::count(&pool, "SELECT count(*) FROM jobs").await, 0);
    Ok(())
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn timeout_is_capped(pool: PgPool) -> anyhow::Result<()> {
    let state = common::state(pool.clone());
    let addr = common::spawn_app(&state).await;

    let response = common::create(
        &state,
        addr,
        &format!("http://{addr}/echo?_timeout=4294967295"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);

    let timeout: i64 = sqlx::query_scalar("SELECT (meta->>'timeout')::bigint FROM jobs")
        .fetch_one(&pool)
        .await?;
    assert_eq!(timeout, i64::from(state.worker_options.max_timeout));
    Ok(())
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn schedule_runs_and_is_deleted(pool: PgPool) -> anyhow::Result<()> {
    let state = common::state(pool.clone());
    let addr = common::spawn_app(&state).await;

    let response = common::create(&state, addr, &format!("http://{addr}/echo?_interval=60")).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let schedule_id = common::header(&response, "schedule-id").expect("schedule-id");

    // first run: completes and schedules the next one
    assert_eq!(common::enqueue_all_scheduled(&state).await, 1);
    assert_eq!(common::run_enqueued(&state).await, 1);
    assert_eq!(
        common::count(&pool, "SELECT count(*) FROM processed").await,
        1
    );
    assert_eq!(
        common::count(&pool, "SELECT count(*) FROM scheduled").await,
        1
    );

    // delete: 204 (was 500 on the foreign key), the pending run is gone, the history stays
    let response = common::request(
        &state,
        Method::DELETE,
        &format!("http://{addr}/api/v1/schedules/{schedule_id}"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        common::count(&pool, "SELECT count(*) FROM scheduled").await,
        0
    );
    assert_eq!(
        common::count(&pool, "SELECT count(*) FROM processed").await,
        1
    );
    assert_eq!(
        common::count(
            &pool,
            "SELECT count(*) FROM jobs WHERE schedule_id IS NOT NULL"
        )
        .await,
        0
    );
    Ok(())
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn running_job_of_deleted_schedule_is_not_cloned(pool: PgPool) -> anyhow::Result<()> {
    let state = common::state(pool.clone());
    let addr = common::spawn_app(&state).await;

    let response = common::create(&state, addr, &format!("http://{addr}/echo?_interval=60")).await;
    let schedule_id = common::header(&response, "schedule-id").expect("schedule-id");
    assert_eq!(common::enqueue_all_scheduled(&state).await, 1);

    // the run is already assigned to a worker when the schedule is deleted
    let jobs = common::fetch(&state, 10).await;
    assert_eq!(jobs.len(), 1);
    let response = common::request(
        &state,
        Method::DELETE,
        &format!("http://{addr}/api/v1/schedules/{schedule_id}"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    for job in jobs {
        irisqo::services::jobrunner::job_run(&state, job).await;
    }

    assert_eq!(
        common::count(&pool, "SELECT count(*) FROM processed").await,
        1
    );
    assert_eq!(common::count(&pool, "SELECT count(*) FROM jobs").await, 1);
    Ok(())
}
