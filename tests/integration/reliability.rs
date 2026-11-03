use std::{sync::Arc, time::Duration};

use hyper::{Method, StatusCode};
use irisqo::{
    db,
    services::{
        jobrunner, retentionservice::RetentionService, schedulerservice::SchedulerService,
        workerservice::WorkerService,
    },
};
use sqlx::PgPool;

use crate::common;

#[sqlx::test(migrations = "./sql/migrations")]
async fn reaper_releases_stuck_lock_and_rejects_late_result(pool: PgPool) -> anyhow::Result<()> {
    let state = common::state(pool.clone());
    let id = common::insert_enqueued(&pool, 1).await[0];
    let stuck = common::fetch(&state, 10).await;
    assert_eq!(stuck.len(), 1);

    // fresh lock: kept
    let lease = Duration::from_secs(60);
    assert_eq!(
        db::jobqueue::reap_expired_locks(&pool, lease, &state.instance_id, 100).await?,
        0
    );
    // stuck for an hour: released with retry + 1
    sqlx::query("UPDATE enqueued SET lock_at = now() - interval '1 hour'")
        .execute(&pool)
        .await?;
    assert_eq!(
        db::jobqueue::reap_expired_locks(&pool, lease, &state.instance_id, 100).await?,
        1
    );
    let (retry, locked): (i32, bool) =
        sqlx::query_as("SELECT retry, lock_at IS NOT NULL FROM enqueued WHERE id = $1")
            .bind(id)
            .fetch_one(&pool)
            .await?;
    assert_eq!((retry, locked), (1, false));
    let (status, message) = common::history(&pool, id).await.pop().expect("history");
    assert_eq!(status, "retried");
    assert!(message.unwrap_or_default().starts_with("lock expired: "));

    // the next attempt is taken by the same instance; then the stuck one finishes late
    let next = common::fetch(&state, 10).await;
    for job in stuck {
        jobrunner::job_run(&state, job).await;
    }
    assert_eq!(common::processed_status(&pool, id).await, None);
    for job in next {
        jobrunner::job_run(&state, job).await;
    }
    assert_eq!(
        common::processed_status(&pool, id).await.as_deref(),
        Some("completed")
    );
    Ok(())
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn expired_instance_is_killed_with_history(pool: PgPool) -> anyhow::Result<()> {
    let state = common::state(pool.clone());
    let id = common::insert_enqueued(&pool, 1).await[0];
    sqlx::query(
        "INSERT INTO instances(instance_id, last_at) VALUES ('old', now() - interval '1 hour')",
    )
    .execute(&pool)
    .await?;
    sqlx::query("UPDATE enqueued SET instance_id = 'old', lock_at = now()")
        .execute(&pool)
        .await?;

    let killed =
        db::instances::kill_expired(&pool, Duration::from_secs(30), &state.instance_id).await?;
    assert_eq!(killed, 1);
    assert_eq!(
        common::count(
            &pool,
            "SELECT count(*) FROM enqueued WHERE lock_at IS NULL AND retry = 1"
        )
        .await,
        1
    );
    assert_eq!(
        common::history(&pool, id).await,
        [("retried".into(), Some("instance expired: old".into()))]
    );
    Ok(())
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn shutdown_release_is_in_history(pool: PgPool) -> anyhow::Result<()> {
    let state = common::state(pool.clone());
    let id = common::insert_enqueued(&pool, 1).await[0];
    assert_eq!(common::fetch(&state, 10).await.len(), 1);

    db::instances::kill(&pool, &state.instance_id).await?;

    assert_eq!(
        common::count(
            &pool,
            "SELECT count(*) FROM enqueued WHERE lock_at IS NULL AND retry = 0"
        )
        .await,
        1
    );
    let (status, message) = common::history(&pool, id).await.pop().expect("history");
    assert_eq!(status, "enqueued");
    assert_eq!(message.as_deref(), Some("released on shutdown"));
    Ok(())
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn scheduler_enqueues_every_due_job(pool: PgPool) -> anyhow::Result<()> {
    let state = common::state(pool.clone());
    common::insert_enqueued(&pool, 2500).await;
    sqlx::query("WITH a AS (DELETE FROM enqueued RETURNING id) INSERT INTO scheduled(id, at) SELECT id, 1 FROM a")
        .execute(&pool)
        .await?;
    let options = state.scheduler_options.as_ref().expect("scheduler options");
    assert!(i64::from(options.prefetch) < 2500);

    SchedulerService::new(Arc::clone(&state))
        .tick(options)
        .await;

    assert_eq!(
        common::count(&pool, "SELECT count(*) FROM scheduled").await,
        0
    );
    assert_eq!(
        common::count(&pool, "SELECT count(*) FROM enqueued").await,
        2500
    );
    Ok(())
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn retention_purges_old_data_only(pool: PgPool) -> anyhow::Result<()> {
    let state = common::state(pool.clone());
    let addr = common::spawn_app(&state).await;
    // old and recent processed jobs, plus a pending one
    common::insert_enqueued(&pool, 3).await;
    common::run_enqueued(&state).await;
    sqlx::query("UPDATE processed SET at = now() - interval '30 days' WHERE id IN (SELECT id FROM processed ORDER BY id LIMIT 2)")
        .execute(&pool)
        .await?;
    // a finished schedule whose only run is old
    let response = common::create(&state, addr, &format!("http://{addr}/echo?_interval=60")).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    common::enqueue_all_scheduled(&state).await;
    sqlx::query("UPDATE schedules SET inactive = true")
        .execute(&pool)
        .await?;
    common::run_enqueued(&state).await;
    sqlx::query("UPDATE processed SET at = now() - interval '30 days' WHERE id IN (SELECT id FROM jobs WHERE schedule_id IS NOT NULL)")
        .execute(&pool)
        .await?;
    let pending = common::insert_enqueued(&pool, 1).await[0];
    // a long dead instance and a live one
    sqlx::query("INSERT INTO instances(instance_id, last_at, status) VALUES ('dead', now() - interval '30 days', 'dead'), ('live', now(), 'live')")
        .execute(&pool)
        .await?;

    RetentionService::new(Arc::clone(&state))
        .purge(Duration::from_secs(7 * 24 * 3600))
        .await;

    assert_eq!(
        common::count(&pool, "SELECT count(*) FROM processed").await,
        1
    );
    // the recent processed job and the pending one
    assert_eq!(common::count(&pool, "SELECT count(*) FROM jobs").await, 2);
    assert_eq!(
        common::count(
            &pool,
            "SELECT count(*) FROM history WHERE id NOT IN (SELECT id FROM jobs)"
        )
        .await,
        0
    );
    assert_eq!(
        common::count(&pool, "SELECT count(*) FROM schedules").await,
        0
    );
    assert_eq!(
        common::count(&pool, "SELECT count(*) FROM instances").await,
        1
    );
    assert_eq!(common::processed_status(&pool, pending).await, None);
    Ok(())
}

/// Creates a job to `/echo?{query}`, runs it once, returns its id.
async fn run_echo(state: &Arc<irisqo::AppState>, query: &str) -> i64 {
    let addr = common::spawn_app(state).await;
    let response = common::create(state, addr, &format!("http://{addr}/echo?{query}")).await;
    assert_eq!(response.status(), StatusCode::CREATED, "{query}");
    let id = common::header(&response, "job-id")
        .and_then(|v| v.parse().ok())
        .expect("job-id");
    assert_eq!(common::run_enqueued(state).await, 1);
    id
}

async fn scheduled_in(pool: &PgPool, id: i64) -> Option<(i32, i64)> {
    sqlx::query_as(
        "SELECT retry, at - extract(epoch from now())::bigint FROM scheduled WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .expect("scheduled")
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn client_error_fails_without_retry(pool: PgPool) -> anyhow::Result<()> {
    let state = common::state(pool.clone());
    let id = run_echo(&state, "status=404&_retry=3|fixed|10").await;
    assert_eq!(
        common::processed_status(&pool, id).await.as_deref(),
        Some("failed")
    );
    assert_eq!(scheduled_in(&pool, id).await, None);
    Ok(())
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn server_error_is_retried_with_jitter(pool: PgPool) -> anyhow::Result<()> {
    let state = common::state(pool.clone());
    let id = run_echo(&state, "status=503&_retry=3|fixed|100").await;
    assert_eq!(common::processed_status(&pool, id).await, None);
    let (retry, delay) = scheduled_in(&pool, id).await.expect("scheduled retry");
    assert_eq!(retry, 1);
    assert!((79..=121).contains(&delay), "{delay}");
    Ok(())
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn too_many_requests_respects_retry_after(pool: PgPool) -> anyhow::Result<()> {
    let state = common::state(pool.clone());
    // immediate retry policy, but the server asks to wait 2 minutes
    let id = run_echo(&state, "status=429&retry-after=120&_retry=3").await;
    let (retry, delay) = scheduled_in(&pool, id).await.expect("scheduled retry");
    assert_eq!(retry, 1);
    assert!((119..=121).contains(&delay), "{delay}");
    Ok(())
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn results_are_written_in_one_batch(pool: PgPool) -> anyhow::Result<()> {
    let state = common::state(pool.clone());
    common::insert_enqueued(&pool, 300).await;
    let mut jobs = common::fetch(&state, 1000).await;
    assert_eq!(jobs.len(), 300);
    // one attempt is stale: its lock was taken over meanwhile
    jobs[0].retry += 1;

    let mut completions = Vec::new();
    for job in jobs {
        completions.extend(jobrunner::job_execute(&state, job).await);
    }
    jobrunner::complete(&state, completions).await;

    assert_eq!(
        common::count(
            &pool,
            "SELECT count(*) FROM processed WHERE status = 'completed'"
        )
        .await,
        299
    );
    assert_eq!(
        common::count(
            &pool,
            "SELECT count(*) FROM history WHERE status = 'completed'"
        )
        .await,
        299
    );
    assert_eq!(
        common::count(&pool, "SELECT count(*) FROM enqueued").await,
        1
    );
    Ok(())
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn worker_service_processes_and_drains_on_shutdown(pool: PgPool) -> anyhow::Result<()> {
    let state = common::state(pool.clone());
    common::insert_enqueued(&pool, 500).await;

    let worker = tokio::spawn({
        let state = Arc::clone(&state);
        async move { WorkerService::new(state).run().await }
    });
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while common::count(&pool, "SELECT count(*) FROM processed").await < 500 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "jobs not processed in time"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    state.shutdown_token.cancel();
    tokio::time::timeout(Duration::from_secs(10), worker).await??;

    assert_eq!(
        common::count(&pool, "SELECT count(*) FROM enqueued").await,
        0
    );
    Ok(())
}

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
