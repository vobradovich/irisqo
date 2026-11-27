use std::{sync::Arc, time::Duration};

use hyper::{Method, StatusCode};
use irisqo::{
    db::{self, jobqueue::ClaimShares},
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

    // a lease that has not ended: kept
    assert_eq!(
        db::jobqueue::reap_expired_locks(&pool, &state.instance_id, 100).await?,
        0
    );
    // the lease ended: released with retry + 1
    sqlx::query("UPDATE claimed SET until = now() - interval '1 second'")
        .execute(&pool)
        .await?;
    assert_eq!(
        db::jobqueue::reap_expired_locks(&pool, &state.instance_id, 100).await?,
        1
    );
    let (attempt, locked): (i32, bool) = sqlx::query_as(
        "SELECT attempt, EXISTS (SELECT 1 FROM claimed WHERE id = $1) FROM enqueued WHERE id = $1",
    )
    .bind(id)
    .fetch_one(&pool)
    .await?;
    assert_eq!((attempt, locked), (1, false));
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
async fn a_held_job_keeps_its_lease_and_a_lost_one_is_released(pool: PgPool) -> anyhow::Result<()> {
    let state = common::state(pool.clone());
    common::insert_enqueued(&pool, 2).await;
    let mut jobs = common::fetch(&state, 10).await;
    assert_eq!(jobs.len(), 2);
    // the worker holds one; the other is lost (its outcome not written, its claim not received)
    let job = jobs.remove(0);
    let held = irisqo::models::JobEntry {
        id: job.job.id,
        attempt: job.attempt,
    };
    state.held.add([held]);
    sqlx::query("UPDATE claimed SET until = now() - interval '1 second'")
        .execute(&pool)
        .await?;

    let (ids, attempts) = state.held.columns();
    let renewed = db::jobqueue::renew_leases(&pool, &state.instance_id, &ids, &attempts).await?;
    assert_eq!(renewed, 1);
    // a stale attempt of the same job renews nothing
    let stale =
        db::jobqueue::renew_leases(&pool, &state.instance_id, &ids, &[held.attempt + 1]).await?;
    assert_eq!(stale, 0);
    assert_eq!(
        db::jobqueue::reap_expired_locks(&pool, &state.instance_id, 100).await?,
        1
    );
    let claimed: Vec<i64> = sqlx::query_scalar("SELECT id FROM claimed")
        .fetch_all(&pool)
        .await?;
    assert_eq!(claimed, [held.id]);

    // once its outcome is written the job is no longer held
    jobrunner::job_run(&state, job).await;
    assert!(state.held.columns().0.is_empty());
    assert_eq!(
        common::processed_status(&pool, held.id).await.as_deref(),
        Some("completed")
    );
    Ok(())
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn expired_instance_is_killed_with_history(pool: PgPool) -> anyhow::Result<()> {
    let state = common::state(pool.clone());
    let id = common::insert_enqueued(&pool, 1).await[0];
    sqlx::query(
        "INSERT INTO instances(instance_id, roles, version, seen_at) VALUES ('old', '{worker}', '1', now() - interval '1 hour')",
    )
    .execute(&pool)
    .await?;
    sqlx::query("WITH a AS (DELETE FROM enqueued RETURNING id, attempt) INSERT INTO claimed(id, attempt, instance_id, until) SELECT id, attempt, 'old', now() + interval '30 seconds' FROM a")
        .execute(&pool)
        .await?;

    let killed =
        db::instances::kill_expired(&pool, Duration::from_secs(30), &state.instance_id).await?;
    assert_eq!(killed, 1);
    assert_eq!(
        common::count(&pool, "SELECT count(*) FROM enqueued WHERE attempt = 1").await,
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
        common::count(&pool, "SELECT count(*) FROM enqueued WHERE attempt = 0").await,
        1
    );
    let (status, message) = common::history(&pool, id).await.pop().expect("history");
    assert_eq!(status, "released");
    assert_eq!(message.as_deref(), Some("released on shutdown"));
    Ok(())
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn scheduler_enqueues_every_due_job(pool: PgPool) -> anyhow::Result<()> {
    let state = common::state(pool.clone());
    common::insert_enqueued(&pool, 2500).await;
    sqlx::query("WITH a AS (DELETE FROM enqueued RETURNING id) INSERT INTO scheduled(id, at) SELECT id, to_timestamp(1) FROM a")
        .execute(&pool)
        .await?;
    assert!(i64::from(state.options.scheduler.prefetch) < 2500);

    SchedulerService::new(Arc::clone(&state)).tick().await;

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
    sqlx::query("UPDATE schedules SET paused = true")
        .execute(&pool)
        .await?;
    common::run_enqueued(&state).await;
    sqlx::query("UPDATE processed SET at = now() - interval '30 days' WHERE id IN (SELECT id FROM jobs WHERE schedule_id IS NOT NULL)")
        .execute(&pool)
        .await?;
    let pending = common::insert_enqueued(&pool, 1).await[0];
    // a long dead instance and a live one
    sqlx::query("INSERT INTO instances(instance_id, roles, version, seen_at, stopped_at) VALUES ('dead', '{worker}', '1', now() - interval '30 days', now() - interval '30 days'), ('live', '{worker}', '1', now(), NULL)")
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
    let id = common::header(&response, "irisqo-job-id")
        .and_then(|v| v.parse().ok())
        .expect("job-id");
    assert_eq!(common::run_enqueued(state).await, 1);
    id
}

async fn scheduled_in(pool: &PgPool, id: i64) -> Option<(i32, i64)> {
    sqlx::query_as(
        "SELECT attempt, (extract(epoch from at) - extract(epoch from now()))::bigint FROM scheduled WHERE id = $1",
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
    let (attempt, delay) = scheduled_in(&pool, id).await.expect("scheduled attempt");
    assert_eq!(attempt, 1);
    assert!((79..=121).contains(&delay), "{delay}");
    Ok(())
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn too_many_requests_respects_retry_after(pool: PgPool) -> anyhow::Result<()> {
    let state = common::state(pool.clone());
    // retries at once, but the server asks to wait 2 minutes
    let id = run_echo(&state, "status=429&retry-after=120&_retry=3").await;
    let (attempt, delay) = scheduled_in(&pool, id).await.expect("scheduled attempt");
    assert_eq!(attempt, 1);
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
    jobs[0].attempt += 1;

    let mut completions = Vec::new();
    for job in jobs {
        completions.push(jobrunner::job_execute(&state, job).await);
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
            "SELECT count(*) FROM history WHERE kind = 'completed'"
        )
        .await,
        299
    );
    // the stale attempt is still claimed
    assert_eq!(
        common::count(&pool, "SELECT count(*) FROM claimed").await,
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

/// Starts a worker that polls an empty queue every 5 s, creates a job through the API while
/// it sleeps: whether the job is done within 2 s.
async fn job_done_before_next_poll(pool: PgPool, args: &[&str]) -> anyhow::Result<bool> {
    let mut args = args.to_vec();
    args.extend(["--interval", "5000", "--max-interval", "5000"]);
    let state = common::state_with(pool.clone(), &args);
    let addr = common::spawn_app(&state).await;
    let worker = tokio::spawn({
        let state = Arc::clone(&state);
        async move { WorkerService::new(state).run().await }
    });
    // the first poll finds nothing: the worker sleeps
    tokio::time::sleep(Duration::from_millis(500)).await;

    let response = common::create(&state, addr, &format!("http://{addr}/echo")).await;
    assert!(response.status().is_success(), "{}", response.status());
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let mut done = false;
    while !done && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
        done = common::count(&pool, "SELECT count(*) FROM processed").await > 0;
    }
    state.shutdown_token.cancel();
    tokio::time::timeout(Duration::from_secs(10), worker).await??;
    Ok(done)
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn a_job_created_by_the_api_wakes_the_worker(pool: PgPool) -> anyhow::Result<()> {
    assert!(job_done_before_next_poll(pool, &[]).await?);
    Ok(())
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn with_no_wake_a_job_waits_for_the_next_poll(pool: PgPool) -> anyhow::Result<()> {
    assert!(!job_done_before_next_poll(pool, &["--no-wake"]).await?);
    Ok(())
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn invalid_schedule_params_are_rejected(pool: PgPool) -> anyhow::Result<()> {
    let state = common::state(pool.clone());
    let addr = common::spawn_app(&state).await;
    let target = format!("http://{addr}/echo");

    for query in [
        "_interval=0",
        "_at=99999999999999",
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
    assert_eq!(timeout, i64::from(state.options.worker.max_timeout));
    Ok(())
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn schedule_runs_and_is_deleted(pool: PgPool) -> anyhow::Result<()> {
    let state = common::state(pool.clone());
    let addr = common::spawn_app(&state).await;

    let response = common::create(&state, addr, &format!("http://{addr}/echo?_interval=60")).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let schedule_id = common::header(&response, "irisqo-schedule-id").expect("schedule-id");

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
    let schedule_id = common::header(&response, "irisqo-schedule-id").expect("schedule-id");
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

fn retries(
    items: &[(
        irisqo::models::JobEntry,
        Option<chrono::DateTime<chrono::Utc>>,
        &str,
    )],
) -> db::jobqueue::Retries {
    let mut retries = db::jobqueue::Retries::default();
    for &(entry, at, message) in items {
        retries.push(entry, at, message.to_owned());
    }
    retries
}

async fn enqueue_in_flow(pool: &PgPool, flow_key: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT enqueue_v1(jsonb_build_object('url', 'http://localhost/x', 'flow_key', $1::text))",
    )
    .bind(flow_key)
    .fetch_one(pool)
    .await
    .expect("enqueue_v1")
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn an_unreadable_job_fails_alone(pool: PgPool) -> anyhow::Result<()> {
    let state = common::state(pool.clone());
    let good = enqueue_in_flow(&pool, "other").await;
    let bad: i64 = sqlx::query_scalar("SELECT enqueue_v1('{\"url\": \"http://localhost/x\"}')")
        .fetch_one(&pool)
        .await?;
    // a URL the worker cannot read
    sqlx::query(
        "UPDATE jobs SET meta = jsonb_set(meta, '{url}', '\"http://localhost/a b\"') WHERE id = $1",
    )
    .bind(bad)
    .execute(&pool)
    .await?;

    let jobs = common::fetch(&state, 10).await;
    let ids: Vec<i64> = jobs.iter().map(|j| j.job.id).collect();
    assert_eq!(ids, [good]);
    assert_eq!(
        common::processed_status(&pool, bad).await.as_deref(),
        Some("failed")
    );
    Ok(())
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn resume_does_not_clone_a_running_run(pool: PgPool) -> anyhow::Result<()> {
    let state = common::state(pool.clone());
    let addr = common::spawn_app(&state).await;
    let target = format!("http://{addr}/echo?_interval=3600");
    let response = common::create(&state, addr, &target).await;
    let schedule_id = common::header(&response, "irisqo-schedule-id").expect("schedule-id");
    common::enqueue_all_scheduled(&state).await;
    assert_eq!(
        common::fetch(&state, 10).await.len(),
        1,
        "the run is in flight"
    );

    let id = &state.instance_id;
    irisqo::features::schedules::pause(&pool, &schedule_id, id).await?;
    irisqo::features::schedules::resume(&pool, &schedule_id, chrono::Utc::now(), id).await?;
    let runs: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs WHERE schedule_id = $1")
        .bind(&schedule_id)
        .fetch_one(&pool)
        .await?;
    assert_eq!(runs, 1);
    Ok(())
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn a_delayed_retry_does_not_hold_its_flow(pool: PgPool) -> anyhow::Result<()> {
    let state = common::state(pool.clone());
    let a = enqueue_in_flow(&pool, "q").await;
    let b = enqueue_in_flow(&pool, "q").await;
    let jobs = common::fetch(&state, 10).await;
    let ids: Vec<i64> = jobs.iter().map(|j| j.job.id).collect();
    assert_eq!(ids, [a], "one at a time");

    let entry = irisqo::models::JobEntry { id: a, attempt: 0 };
    let at = chrono::Utc::now() + chrono::Duration::seconds(60);
    let moved = db::jobqueue::retry_many(
        &pool,
        &state.instance_id,
        &retries(&[(entry, Some(at), "again")]),
    )
    .await?;
    assert_eq!(moved, 1);
    assert_eq!(
        common::count(&pool, "SELECT count(*) FROM scheduled").await,
        1
    );
    let next: Vec<i64> = common::fetch(&state, 10)
        .await
        .iter()
        .map(|j| j.job.id)
        .collect();
    assert_eq!(next, [b], "the flow is free while the retry waits");
    // a late result of the same attempt moves nothing
    let again = db::jobqueue::retry_many(
        &pool,
        &state.instance_id,
        &retries(&[(entry, Some(at), "again")]),
    )
    .await?;
    assert_eq!(again, 0);
    Ok(())
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn retries_are_written_in_one_batch(pool: PgPool) -> anyhow::Result<()> {
    let state = common::state(pool.clone());
    common::insert_enqueued(&pool, 3).await;
    let jobs = common::fetch(&state, 10).await;
    let entries: Vec<_> = jobs
        .iter()
        .map(|j| irisqo::models::JobEntry {
            id: j.job.id,
            attempt: j.attempt,
        })
        .collect();
    let at = chrono::Utc::now() + chrono::Duration::seconds(60);
    // at once, later, and a stale attempt that moves nothing
    let stale = irisqo::models::JobEntry {
        attempt: entries[2].attempt + 1,
        ..entries[2]
    };
    let batch = retries(&[
        (entries[0], None, "now"),
        (entries[1], Some(at), "later"),
        (stale, None, "stale"),
    ]);
    assert_eq!(
        db::jobqueue::retry_many(&pool, &state.instance_id, &batch).await?,
        2
    );
    let moved: Vec<(i64, i32, bool)> = sqlx::query_as(
        "SELECT id, attempt, false FROM enqueued UNION ALL SELECT id, attempt, true FROM scheduled ORDER BY id",
    )
    .fetch_all(&pool)
    .await?;
    assert_eq!(moved, [(entries[0].id, 1, false), (entries[1].id, 1, true)]);
    assert_eq!(
        common::history(&pool, entries[1].id).await.pop(),
        Some(("retried".into(), Some("later".into())))
    );
    assert_eq!(
        common::count(&pool, "SELECT count(*) FROM claimed").await,
        1
    );
    Ok(())
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn a_flow_gives_the_highest_priority_first_then_new_before_retried(
    pool: PgPool,
) -> anyhow::Result<()> {
    let state = common::state(pool.clone());
    sqlx::query("INSERT INTO flows(flow_id, concurrency) VALUES ('q', 8)")
        .execute(&pool)
        .await?;
    let retried = enqueue_in_flow(&pool, "q").await;
    let low = enqueue_in_flow(&pool, "q").await;
    let high = enqueue_in_flow(&pool, "q").await;
    sqlx::query("UPDATE enqueued SET attempt = 1 WHERE id = $1")
        .bind(retried)
        .execute(&pool)
        .await?;
    sqlx::query("UPDATE enqueued SET priority = 5 WHERE id = $1")
        .bind(high)
        .execute(&pool)
        .await?;
    let ids: Vec<i64> = common::fetch(&state, 10)
        .await
        .iter()
        .map(|j| j.job.id)
        .collect();
    assert_eq!(ids, [high, low, retried]);
    Ok(())
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn enqueue_v1_priority_is_an_int_and_goes_with_a_flow(pool: PgPool) -> anyhow::Result<()> {
    for (extra, message) in [(r#""priority": 3000000000"#, "priority must be from")] {
        let job = format!(r#"{{"url":"noop://x", {extra}}}"#);
        let err = sqlx::query_scalar::<_, i64>("SELECT enqueue_v1($1::jsonb)")
            .bind(job)
            .fetch_one(&pool)
            .await
            .expect_err("rejected");
        assert!(err.to_string().contains(message), "{err}");
    }
    // a flow keeps the priority of its job
    sqlx::query_scalar::<_, i64>(
        r#"SELECT enqueue_v1('{"url":"noop://x", "flow_key": "q", "priority": 7}')"#,
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        common::count(
            &pool,
            "SELECT count(*) FROM enqueued WHERE flow_key = 'q' AND priority = 7"
        )
        .await,
        1
    );
    Ok(())
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn enqueue_v1_checks_the_form_of_key_and_flow(pool: PgPool) -> anyhow::Result<()> {
    for (field, bad) in [("key", "two\nlines"), ("flow_key", "no spaces")] {
        let job = serde_json::json!({ "url": "http://localhost/x", field: bad }).to_string();
        let err = sqlx::query_scalar::<_, i64>("SELECT enqueue_v1($1::jsonb)")
            .bind(job)
            .fetch_one(&pool)
            .await
            .expect_err("rejected");
        assert!(
            err.to_string().contains(&format!("{field} must be")),
            "{err}"
        );
    }
    sqlx::query_scalar::<_, i64>(
        "SELECT enqueue_v1('{\"url\": \"http://localhost/x\", \"key\": \"Order 1/abc=\", \"flow_key\": \"q:1\"}')",
    )
    .fetch_one(&pool)
    .await?;
    Ok(())
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn enqueue_v1_stores_the_retry_policy(pool: PgPool) -> anyhow::Result<()> {
    use irisqo::models::{JobMeta, JobRetry};
    use serde_json::json;

    let pool = &pool;
    let enqueue = |job: serde_json::Value| async move {
        let id: i64 = sqlx::query_scalar("SELECT enqueue_v1($1::jsonb)")
            .bind(job.to_string())
            .fetch_one(pool)
            .await?;
        let meta: sqlx::types::Json<JobMeta> =
            sqlx::query_scalar("SELECT meta FROM jobs WHERE id = $1")
                .bind(id)
                .fetch_one(pool)
                .await?;
        Ok::<_, sqlx::Error>(meta.0.retry)
    };
    let url = "http://localhost/x";

    assert_eq!(
        enqueue(json!({"url": url, "retry": {"policy": "fixed", "count": 3}})).await?,
        Some(JobRetry::Fixed { count: 3, delay: 0 })
    );
    assert_eq!(
        enqueue(
            json!({"url": url, "retry": {"policy": "exponential", "count": 5,
            "delay": 2, "max_delay": 60}})
        )
        .await?,
        Some(JobRetry::Exponential {
            count: 5,
            delay: 2,
            max_delay: Some(60)
        })
    );
    assert_eq!(enqueue(json!({"url": url })).await?, None);
    // no retries, said out loud
    assert_eq!(
        enqueue(json!({"url": url, "retry": {"policy": "fixed", "count": 0}})).await?,
        Some(JobRetry::NONE)
    );
    assert_eq!(
        enqueue(
            json!({"url": url, "retry": {"policy": "fixed", "count": 9999,
            "delay": 604_800}})
        )
        .await?,
        Some(JobRetry::Fixed {
            count: 9999,
            delay: 604_800
        })
    );

    for (job, error) in [
        (
            json!({"url": url, "retry": {"policy": "fixd", "count": 3}}),
            "retry.policy must be",
        ),
        // the text of `_retry` is for the API, not for SQL
        (
            json!({"url": url, "retry": "5|exponential|2"}),
            "retry must be an object",
        ),
        (
            json!({"url": url, "callback": {"url": url, "retry": "none"}}),
            "callback.retry must be an object",
        ),
        (
            json!({"url": url, "retry": {"policy": "fixed", "count": 10000}}),
            "retry.count must be",
        ),
        (
            json!({"url": url, "retry": {"policy": "fixed", "count": 1,
                "delay": 604_801}}),
            "retry.delay must be",
        ),
        (
            json!({"url": url, "retry": {"policy": "exponential", "count": 3}}),
            "retry.delay must be",
        ),
        (
            json!({"url": url, "retry": {"policy": "fibonacci", "count": 3,
                "delay": 1, "max_delay": "x"}}),
            "retry.max_delay must be",
        ),
        (
            json!({"url": url, "retry": {"policy": "fixed", "count": 3,
                "max_delay": 9}}),
            "retry.max_delay is for",
        ),
        (
            json!({"url": url, "retry": {"policy": "fixed", "count": 3,
                "backoff": 2}}),
            "retry may have policy, count, delay and max_delay only",
        ),
        (
            json!({"url": url, "retry": {"policy": "fixed", "count": "3"}}),
            "retry.count must be",
        ),
        (
            json!({"url": url, "retry": {"policy": "exponential", "count": 3,
                "delay": "2"}}),
            "retry.delay must be",
        ),
        (
            json!({"url": url, "callback": {"url": url,
                "retry": {"policy": "fixed", "count": 3, "backoff": 2}}}),
            "callback.retry may have policy, count, delay and max_delay only",
        ),
        (
            json!({"url": url, "callback": {"url": url,
                "retry": {"policy": "fixed", "count": "3"}}}),
            "callback.retry.count must be",
        ),
    ] {
        let err = enqueue(job).await.expect_err("rejected");
        assert!(err.to_string().contains(error), "{err}");
    }
    Ok(())
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn a_released_or_requeued_job_keeps_its_priority_and_flow(
    pool: PgPool,
) -> anyhow::Result<()> {
    let state = common::state(pool.clone());
    let id = &state.instance_id;
    let urgent: i64 = sqlx::query_scalar(
        "SELECT enqueue_v1('{\"url\": \"http://localhost/x\", \"priority\": 5}')",
    )
    .fetch_one(&pool)
    .await?;
    let queued = enqueue_in_flow(&pool, "q").await;
    let sql = "SELECT id, flow_key, priority FROM enqueued ORDER BY id";
    let want = vec![(urgent, None, 5_i32), (queued, Some("q".to_owned()), 0)];

    // claim, then release on shutdown
    assert_eq!(common::fetch(&state, 10).await.len(), 2);
    db::instances::kill(&pool, id).await?;
    assert_eq!(
        sqlx::query_as::<_, (i64, Option<String>, i32)>(sql)
            .fetch_all(&pool)
            .await?,
        want
    );

    // claim, then an immediate retry of one job
    let jobs = common::fetch(&state, 10).await;
    let entry = irisqo::models::JobEntry {
        id: urgent,
        attempt: jobs[0].attempt,
    };
    assert_eq!(
        db::jobqueue::retry_many(&pool, id, &retries(&[(entry, None, "again")])).await?,
        1
    );
    let rows = sqlx::query_as::<_, (i64, Option<String>, i32)>(sql)
        .fetch_all(&pool)
        .await?;
    assert_eq!(rows, vec![want[0].clone()]);

    // processed, then run once more by a user
    sqlx::query("WITH a AS (DELETE FROM claimed RETURNING id, attempt) INSERT INTO processed(id, attempt, instance_id, status) SELECT id, attempt, 'x', 'failed' FROM a")
        .execute(&pool)
        .await?;
    for job in [urgent, queued] {
        db::jobqueue::requeue_job(&pool, job, id).await?;
    }
    let rows = sqlx::query_as::<_, (i64, Option<String>, i32)>(sql)
        .fetch_all(&pool)
        .await?;
    assert_eq!(rows, want);
    Ok(())
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn a_schedule_with_a_running_run_is_not_idle(pool: PgPool) -> anyhow::Result<()> {
    let state = common::state(pool.clone());
    let addr = common::spawn_app(&state).await;
    let target = format!("http://{addr}/echo?_interval=3600");
    common::create(&state, addr, &target).await;
    common::enqueue_all_scheduled(&state).await;
    assert_eq!(
        common::fetch(&state, 10).await.len(),
        1,
        "the run is in flight"
    );

    let views = irisqo::features::schedules::list_view(&pool, 10).await?;
    assert_eq!(views.len(), 1);
    assert!(views[0].pending);
    Ok(())
}

/// The tests above run in `public` (`#[sqlx::test]` migrates there). This one starts the app
/// the way `main` does: its own schema, created and migrated by `AppState::new`.
#[sqlx::test(migrations = false)]
async fn the_app_creates_and_uses_its_own_schema(pool: PgPool) -> anyhow::Result<()> {
    let url = common::database_url(&pool);
    let mut options = irisqo::models::Options::new(url.to_string());
    options.db.schema = "mine".to_owned();
    options.environment = irisqo::models::Environment::Development;
    let state = irisqo::AppState::new(options).await;

    let (inside, outside): (i64, i64) = sqlx::query_as(
        "SELECT count(*) FILTER (WHERE table_schema = 'mine'),
                count(*) FILTER (WHERE table_schema = 'public')
         FROM information_schema.tables",
    )
    .fetch_one(&pool)
    .await?;
    assert!(inside >= 10, "the tables are in the schema: {inside}");
    assert_eq!(outside, 0, "nothing in public");

    // the functions find the tables of their schema
    let id: i64 = sqlx::query_scalar("SELECT enqueue_v1('{\"url\": \"http://localhost/x\"}')")
        .fetch_one(&state.pool)
        .await?;
    let jobs = common::fetch(&state, 10).await;
    assert_eq!(jobs.iter().map(|j| j.job.id).collect::<Vec<_>>(), [id]);
    Ok(())
}

/// `--no-create-schema`: a role without `CREATE` on the database cannot run even
/// `CREATE SCHEMA IF NOT EXISTS` on an existing schema, and migrates it with the flag.
#[sqlx::test(migrations = false)]
async fn a_role_without_create_migrates_an_existing_schema(pool: PgPool) -> anyhow::Result<()> {
    use irisqo::db::setup;

    // roles are of the cluster and outlive a failed run; sqlx drops the database of the test before
    // the next one, so the role owns nothing then and goes
    let role = "irisqo_test_no_create";
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "DROP ROLE IF EXISTS \"{role}\";
         CREATE SCHEMA given;
         CREATE ROLE \"{role}\" LOGIN PASSWORD 'secret';
         GRANT USAGE, CREATE ON SCHEMA given TO \"{role}\";"
    )))
    .execute(&pool)
    .await?;
    let mut url = common::database_url(&pool);
    url.set_username(role).expect("username");
    url.set_password(Some("secret")).expect("password");
    let mut db = irisqo::models::DbOptions::new(url.to_string());
    db.schema = "given".to_owned();

    let creating = setup::connect(&db, "test").await;
    let refused = tokio::spawn({
        let db = db.clone();
        async move { setup::migrate_schema(&creating, &db).await }
    })
    .await
    .expect_err("CREATE on the database is needed");
    let message = refused.into_panic();
    let message = message.downcast_ref::<String>().map_or("", String::as_str);
    assert!(
        message.contains("permission denied for database"),
        "{message}"
    );

    db.create_schema = false;
    let existing = setup::connect(&db, "test").await;
    setup::migrate_schema(&existing, &db).await;
    existing.close().await;

    let tables = common::count(
        &pool,
        "SELECT count(*) FROM information_schema.tables WHERE table_schema = 'given'",
    )
    .await;
    assert!(tables >= 9, "the tables are in the schema: {tables}");
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "DROP OWNED BY \"{role}\"; DROP ROLE \"{role}\";"
    )))
    .execute(&pool)
    .await?;
    Ok(())
}

/// The script of `irisqo migrations sql` builds the schema on its own, like `irisqo migrate`.
#[sqlx::test(migrations = false)]
async fn the_printed_migrations_build_the_schema(pool: PgPool) -> anyhow::Result<()> {
    let script = irisqo::db::setup::migrations_sql("printed", true);
    sqlx::raw_sql(sqlx::AssertSqlSafe(script))
        .execute(&pool)
        .await?;

    let tables: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM information_schema.tables WHERE table_schema = 'printed'",
    )
    .fetch_one(&pool)
    .await?;
    assert!(tables >= 9, "the tables are in the schema: {tables}");
    // the functions find the tables of their schema
    let id: i64 = sqlx::query_scalar("SELECT enqueue_v1('{\"url\": \"http://localhost/x\"}')")
        .fetch_one(&pool)
        .await?;
    assert!(id > 0);
    Ok(())
}

/// `claim_v1` takes a fresh snapshot of its own: a job enqueued while a claim runs can be
/// claimed although the snapshot of the caller does not see its row in `jobs` yet.
#[sqlx::test(migrations = "./sql/migrations")]
async fn a_job_enqueued_during_a_claim_is_not_lost(pool: PgPool) -> anyhow::Result<()> {
    use std::sync::atomic::{AtomicBool, Ordering};

    let state = common::state(pool.clone());
    let stop = Arc::new(AtomicBool::new(false));
    let writers: Vec<_> = (0..4)
        .map(|_| {
            let (pool, stop) = (pool.clone(), Arc::clone(&stop));
            tokio::spawn(async move {
                while !stop.load(Ordering::Relaxed) {
                    sqlx::query("SELECT enqueue_v1('{\"url\": \"http://localhost/x\"}')")
                        .execute(&pool)
                        .await
                        .expect("enqueue_v1");
                }
            })
        })
        .collect();

    let mut returned = 0;
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while std::time::Instant::now() < deadline {
        returned += common::fetch(&state, 16).await.len();
    }
    stop.store(true, Ordering::Relaxed);
    for writer in writers {
        writer.await?;
    }

    let claimed = common::count(&pool, "SELECT count(*) FROM claimed").await;
    assert!(returned > 0);
    assert_eq!(
        i64::try_from(returned)?,
        claimed,
        "every claimed job is handed to a worker"
    );
    Ok(())
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn jobs_without_a_flow_come_first_and_the_queues_get_the_rest(
    pool: PgPool,
) -> anyhow::Result<()> {
    let state = common::state(pool.clone());
    let queued = enqueue_in_flow(&pool, "q").await;
    let plain = common::insert_enqueued(&pool, 2).await;
    let urgent: i64 = sqlx::query_scalar(
        "SELECT enqueue_v1('{\"url\": \"http://localhost/x\", \"priority\": 5}')",
    )
    .fetch_one(&pool)
    .await?;

    // the batch is full of jobs without a flow: by priority, then by id; the flow waits
    let ids = |jobs: Vec<irisqo::models::JobWithAttempt>| {
        jobs.iter().map(|j| j.job.id).collect::<Vec<_>>()
    };
    assert_eq!(
        ids(common::fetch(&state, 3).await),
        [urgent, plain[0], plain[1]]
    );
    assert_eq!(
        common::count(&pool, "SELECT count(*) FROM claimed WHERE flow_key = 'q'").await,
        0
    );

    // the flow gets the room that is left
    let more = enqueue_in_flow(&pool, "other").await;
    assert_eq!(ids(common::fetch(&state, 5).await), [queued, more]);
    Ok(())
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn a_full_flow_does_not_cost_its_backlog(pool: PgPool) -> anyhow::Result<()> {
    let state = common::state(pool.clone());
    sqlx::query("INSERT INTO flows(flow_id, concurrency) VALUES ('q', 8)")
        .execute(&pool)
        .await?;
    sqlx::query(
        "WITH a AS (INSERT INTO jobs(meta) SELECT jsonb_build_object('timeout', 10000, 'sink', 'noop') FROM generate_series(1, 100000) RETURNING id)
         INSERT INTO enqueued(id, flow_key) SELECT id, 'q' FROM a",
    )
    .execute(&pool)
    .await?;
    sqlx::query("ANALYZE enqueued").execute(&pool).await?;

    // the flow gives its places and no more
    assert_eq!(common::fetch(&state, 64).await.len(), 8);
    let started = std::time::Instant::now();
    assert!(common::fetch(&state, 64).await.is_empty());
    assert!(
        started.elapsed() < Duration::from_millis(100),
        "the claim read the backlog: {:?}",
        started.elapsed()
    );
    Ok(())
}

/// Inserts `n` noop jobs of the flow `flow` straight into `enqueued`.
async fn insert_in_flow(pool: &PgPool, flow: &str, n: i32) {
    sqlx::query(
        "WITH a AS (
            INSERT INTO jobs(meta) SELECT '{\"sink\": \"noop\", \"timeout\": 1000}'::jsonb
            FROM generate_series(1, $2) RETURNING id
        )
        INSERT INTO enqueued(id, flow_key) SELECT id, $1 FROM a",
    )
    .bind(flow)
    .bind(n)
    .execute(pool)
    .await
    .expect("insert in flow");
}

/// The claimed jobs by flow, by name (`None` for the jobs without a flow).
async fn claimed_by_flow(pool: &PgPool) -> Vec<(Option<String>, i64)> {
    sqlx::query_as("SELECT flow_key, count(*) FROM claimed GROUP BY 1 ORDER BY 1")
        .fetch_all(pool)
        .await
        .expect("claimed by flow")
}

const FLOWS_FIRST: ClaimShares = ClaimShares {
    flows: 100,
    declared: 100,
};
const UNDECLARED_FIRST: ClaimShares = ClaimShares {
    flows: 100,
    declared: 0,
};

#[sqlx::test(migrations = "./sql/migrations")]
async fn a_backlog_without_a_flow_does_not_starve_the_flows(pool: PgPool) -> anyhow::Result<()> {
    let state = common::state(pool.clone());
    common::insert_enqueued(&pool, 2000).await;
    enqueue_in_flow(&pool, "q").await;

    // the default shares: the flows go first in about one claim of four
    let mut claims = 0;
    while common::count(&pool, "SELECT count(*) FROM claimed WHERE flow_key = 'q'").await == 0 {
        assert!(!common::fetch_with(&state, 10, None).await.is_empty());
        claims += 1;
    }
    let plain_left = common::count(&pool, "SELECT count(*) FROM enqueued").await;
    assert!(plain_left > 0, "the flow waited for the whole backlog");
    assert!(claims < 100, "{claims} claims");
    Ok(())
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn the_declared_flows_share_the_places_by_concurrency(pool: PgPool) -> anyhow::Result<()> {
    let state = common::state(pool.clone());
    sqlx::query("INSERT INTO flows(flow_id, concurrency) VALUES ('big', 100), ('small', 10)")
        .execute(&pool)
        .await?;
    insert_in_flow(&pool, "big", 200).await;
    insert_in_flow(&pool, "small", 200).await;

    // 64 places, 100 + 10 free: both fill to the same share of their concurrency
    common::fetch_with(&state, 64, Some(FLOWS_FIRST)).await;
    assert_eq!(
        claimed_by_flow(&pool).await,
        [(Some("big".into()), 59), (Some("small".into()), 5)]
    );
    // the next claim tops up the less loaded one first
    common::fetch_with(&state, 10, Some(FLOWS_FIRST)).await;
    assert_eq!(
        claimed_by_flow(&pool).await,
        [(Some("big".into()), 68), (Some("small".into()), 6)]
    );
    Ok(())
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn the_undeclared_part_leaves_the_declared_flows_alone(pool: PgPool) -> anyhow::Result<()> {
    let state = common::state(pool.clone());
    sqlx::query("INSERT INTO flows(flow_id, concurrency, paused) VALUES ('d', 2, true)")
        .execute(&pool)
        .await?;
    insert_in_flow(&pool, "d", 5).await;
    insert_in_flow(&pool, "u", 5).await;

    // a paused declared flow gives nothing, whichever part goes first
    common::fetch_with(&state, 10, Some(UNDECLARED_FIRST)).await;
    assert_eq!(claimed_by_flow(&pool).await, [(Some("u".into()), 1)]);
    // resumed, it gives its concurrency and no more
    sqlx::query("UPDATE flows SET paused = false")
        .execute(&pool)
        .await?;
    common::fetch_with(&state, 10, Some(UNDECLARED_FIRST)).await;
    assert_eq!(
        claimed_by_flow(&pool).await,
        [(Some("d".into()), 2), (Some("u".into()), 1)]
    );
    Ok(())
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn a_claim_tells_how_long_each_job_waited_and_its_class(pool: PgPool) -> anyhow::Result<()> {
    sqlx::query("INSERT INTO flows(flow_id, concurrency) VALUES ('d', 5)")
        .execute(&pool)
        .await?;
    insert_in_flow(&pool, "d", 2).await;
    insert_in_flow(&pool, "u", 1).await;
    common::insert_enqueued(&pool, 1).await;
    sqlx::query("UPDATE enqueued SET at = now() - interval '10 seconds'")
        .execute(&pool)
        .await?;

    let rows: Vec<(Option<bool>, f64)> = sqlx::query_as(
        "SELECT flow_declared, waited FROM claim_v1('i', 10, interval '30 seconds', 100, 100)
         ORDER BY 1 NULLS FIRST",
    )
    .fetch_all(&pool)
    .await?;
    let declared: Vec<_> = rows.iter().map(|r| r.0).collect();
    assert_eq!(declared, [None, Some(false), Some(true), Some(true)]);
    assert!(rows.iter().all(|r| (10.0..11.0).contains(&r.1)), "{rows:?}");
    Ok(())
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn the_undeclared_flows_take_turns(pool: PgPool) -> anyhow::Result<()> {
    let state = common::state(pool.clone());
    for name in ["k0", "k1", "k2", "k3", "k4", "k5"] {
        insert_in_flow(&pool, name, 2).await;
    }
    irisqo::features::instances::live(&pool, &state.instance_id, &["worker"]).await?;
    let mut turns = Vec::new();
    for _ in 0..4 {
        common::fetch_with(&state, 2, Some(UNDECLARED_FIRST)).await;
        let names: Vec<String> = sqlx::query_scalar(
            "WITH d AS (DELETE FROM claimed RETURNING flow_key) SELECT flow_key FROM d ORDER BY 1",
        )
        .fetch_all(&pool)
        .await?;
        turns.push((names, common::flow_cursor(&state).await));
    }
    let turn = |a: &str, b: &str| (vec![a.to_owned(), b.to_owned()], Some(b.to_owned()));
    assert_eq!(
        turns,
        [
            turn("k0", "k1"),
            turn("k2", "k3"),
            turn("k4", "k5"),
            turn("k0", "k1")
        ]
    );
    Ok(())
}

/// Moves the first job of every flow to `claimed` of the instance `x` that never ends: each
/// flow is busy and keeps its other jobs waiting.
async fn hold_one_job_per_flow(pool: &PgPool) {
    sqlx::query(
        "WITH r AS (SELECT DISTINCT ON (flow_key) id FROM enqueued WHERE flow_key IS NOT NULL
                    ORDER BY flow_key, id),
              d AS (DELETE FROM enqueued e USING r WHERE e.id = r.id RETURNING e.id, e.flow_key)
         INSERT INTO claimed(id, attempt, flow_key, instance_id, until)
         SELECT id, 0, flow_key, 'x', now() + interval '1 hour' FROM d",
    )
    .execute(pool)
    .await
    .expect("hold one job per flow");
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn the_walk_over_busy_undeclared_flows_moves_on_without_a_claim(
    pool: PgPool,
) -> anyhow::Result<()> {
    let state = common::state(pool.clone());
    irisqo::features::instances::live(&pool, &state.instance_id, &["worker"]).await?;
    for name in ["a1", "a2", "a3", "a4", "a5", "a6", "a7", "a8", "a9"] {
        insert_in_flow(&pool, name, 2).await;
    }
    hold_one_job_per_flow(&pool).await;
    insert_in_flow(&pool, "z", 1).await;

    // a claim of 1 looks at 4 names at most; an empty one still moves the cursor
    let mut turns = Vec::new();
    for _ in 0..3 {
        let jobs = common::fetch_with(&state, 1, Some(UNDECLARED_FIRST)).await;
        turns.push((jobs.len(), common::flow_cursor(&state).await));
    }
    let turn = |n: usize, after: &str| (n, Some(after.to_owned()));
    assert_eq!(turns, [turn(0, "a4"), turn(0, "a8"), turn(1, "z")]);
    Ok(())
}

/// Mean milliseconds of `n` claims of 64 that try the undeclared flows first.
async fn mean_claim_ms(state: &irisqo::AppState, n: u32) -> f64 {
    let start = std::time::Instant::now();
    for _ in 0..n {
        common::fetch_with(state, 64, Some(UNDECLARED_FIRST)).await;
    }
    start.elapsed().as_secs_f64() * 1000.0 / f64::from(n)
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn a_claim_does_not_slow_down_with_thousands_of_busy_undeclared_flows(
    pool: PgPool,
) -> anyhow::Result<()> {
    let state = common::state(pool.clone());
    irisqo::features::instances::live(&pool, &state.instance_id, &["worker"]).await?;
    common::insert_enqueued(&pool, 64 * 60).await;
    sqlx::query(
        "WITH a AS (INSERT INTO jobs(meta) SELECT '{\"sink\": \"noop\", \"timeout\": 1000}'::jsonb
                    FROM generate_series(1, 20000) RETURNING id)
         INSERT INTO enqueued(id, flow_key) SELECT id, 'f' || lpad((id % 10000)::text, 5, '0') FROM a",
    )
    .execute(&pool)
    .await?;
    hold_one_job_per_flow(&pool).await;
    sqlx::query("ANALYZE").execute(&pool).await?;

    mean_claim_ms(&state, 3).await;
    let busy = mean_claim_ms(&state, 20).await;
    assert_eq!(
        common::count(
            &pool,
            "SELECT count(*) FROM claimed WHERE instance_id <> 'x' AND flow_key IS NOT NULL"
        )
        .await,
        0
    );
    sqlx::raw_sql("DELETE FROM enqueued WHERE flow_key IS NOT NULL; DELETE FROM claimed WHERE instance_id = 'x'")
        .execute(&pool)
        .await?;
    let idle = mean_claim_ms(&state, 20).await;
    // a walk over all 10 000 busy names took ~65 ms a claim
    assert!(
        busy < idle * 2.0 + 3.0,
        "busy {busy:.1} ms, idle {idle:.1} ms"
    );
    Ok(())
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn concurrent_claims_start_one_job_of_an_undeclared_flow(pool: PgPool) -> anyhow::Result<()> {
    let state = common::state(pool.clone());
    insert_in_flow(&pool, "u", 20).await;
    for _ in 0..5 {
        let claims: Vec<_> = (0..8)
            .map(|_| {
                let state = Arc::clone(&state);
                tokio::spawn(async move {
                    common::fetch_with(&state, 10, Some(UNDECLARED_FIRST))
                        .await
                        .len()
                })
            })
            .collect();
        let mut started = 0;
        for claim in claims {
            started += claim.await?;
        }
        assert_eq!(started, 1);
        assert_eq!(claimed_by_flow(&pool).await, [(Some("u".into()), 1)]);
        sqlx::query("DELETE FROM claimed").execute(&pool).await?;
    }
    Ok(())
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn a_paused_flow_gives_nothing_and_a_rate_spaces_the_starts(
    pool: PgPool,
) -> anyhow::Result<()> {
    let state = common::state(pool.clone());
    sqlx::query(
        "INSERT INTO flows(flow_id, concurrency, rate, rate_period) VALUES ('q', 8, 2, 3600)",
    )
    .execute(&pool)
    .await?;
    for _ in 0..5 {
        enqueue_in_flow(&pool, "q").await;
    }
    // two starts per hour: the rest waits for `next_at`
    assert_eq!(common::fetch(&state, 10).await.len(), 2);
    assert!(common::fetch(&state, 10).await.is_empty());
    sqlx::query("UPDATE flows SET next_at = NULL, paused = true WHERE flow_id = 'q'")
        .execute(&pool)
        .await?;
    assert!(common::fetch(&state, 10).await.is_empty());
    sqlx::query("UPDATE flows SET paused = false WHERE flow_id = 'q'")
        .execute(&pool)
        .await?;
    assert_eq!(common::fetch(&state, 10).await.len(), 2);
    Ok(())
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn flow_rate_free_counts_the_starts_a_bucket_holds(pool: PgPool) -> anyhow::Result<()> {
    // no rate, idle, a year idle, half the debt, one start short of a refill, a full debt
    let free: (i32, i32, i32, i32, i32, i32) = sqlx::query_as(
        "SELECT flow_rate_free_v1(NULL, 60, NULL),
                flow_rate_free_v1(10, 60, NULL),
                flow_rate_free_v1(10, 60, now() - interval '1 year'),
                flow_rate_free_v1(10, 60, now() + interval '30 seconds'),
                flow_rate_free_v1(10, 60, now() + interval '59 seconds'),
                flow_rate_free_v1(10, 60, now() + interval '1 hour')",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(free, (i32::MAX, 10, 10, 5, 0, 0));
    Ok(())
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn a_rate_is_a_bucket_of_one_period_that_refills_one_start_at_a_time(
    pool: PgPool,
) -> anyhow::Result<()> {
    let state = common::state(pool.clone());
    // 10 per minute: a start every 6 s, a full bucket holds 10
    sqlx::query(
        "INSERT INTO flows(flow_id, concurrency, rate, rate_period) VALUES ('q', 100, 10, 60)",
    )
    .execute(&pool)
    .await?;
    for _ in 0..25 {
        enqueue_in_flow(&pool, "q").await;
    }
    // idle: the whole bucket at once, no more
    assert_eq!(common::fetch(&state, 64).await.len(), 10);
    assert!(common::fetch(&state, 64).await.is_empty());
    // 6 s of the debt paid off: one start is free again, and costs 6 s
    sqlx::query("UPDATE flows SET next_at = now() + interval '54 seconds' WHERE flow_id = 'q'")
        .execute(&pool)
        .await?;
    assert_eq!(common::fetch(&state, 64).await.len(), 1);
    assert!(common::fetch(&state, 64).await.is_empty());
    // a long idle time gives no more than a full bucket
    sqlx::query("UPDATE flows SET next_at = now() - interval '1 hour' WHERE flow_id = 'q'")
        .execute(&pool)
        .await?;
    assert_eq!(common::fetch(&state, 64).await.len(), 10);
    Ok(())
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn enqueue_v1_sink_comes_from_the_url_and_at_excludes_delay(
    pool: PgPool,
) -> anyhow::Result<()> {
    let enqueue = |job: serde_json::Value| {
        sqlx::query_scalar::<_, i64>("SELECT enqueue_v1($1::jsonb)")
            .bind(job.to_string())
            .fetch_one(&pool)
    };
    let meta = |id: i64| {
        sqlx::query_scalar::<_, serde_json::Value>("SELECT meta FROM jobs WHERE id = $1")
            .bind(id)
            .fetch_one(&pool)
    };
    for (job, error) in [
        (serde_json::json!({}), "url must be"),
        (
            serde_json::json!({"sink": "noop", "url": "noop://x"}),
            "fields that are not known: sink",
        ),
        // a field that is not known, of the job or of its callback
        (
            serde_json::json!({"url":"noop://x", "callback_url": "http://localhost/cb"}),
            "fields that are not known: callback_url",
        ),
        (
            serde_json::json!({"url":"noop://x", "callback": {"url": "http://localhost/cb",
                "retries": 3}}),
            "callback fields that are not known: retries",
        ),
        // a body is a body or base64, whatever its JSON
        (
            serde_json::json!({"url":"noop://x", "body": {"a": 1}, "body_base64": "YQ=="}),
            "body and body_base64 are exclusive",
        ),
        // a time that is one
        (
            serde_json::json!({"url":"noop://x", "at": "2030-02-30T00:00:00Z"}),
            "at must be",
        ),
        (
            serde_json::json!({"url":"noop://x", "at": "1969-12-31T00:00:00Z"}),
            "at must be",
        ),
        (
            serde_json::json!({"url":"noop://x", "at": "tomorrow"}),
            "at must be",
        ),
    ] {
        let err = enqueue(job).await.expect_err(error);
        assert!(err.to_string().contains(error), "{err}");
    }
    // the sink comes from the url, the method is POST
    let id = enqueue(serde_json::json!({"url": "http://localhost/x"})).await?;
    let m = meta(id).await?;
    assert_eq!(
        (m["sink"].as_str(), m["method"].as_str()),
        (Some("http"), Some("POST"))
    );
    let id = enqueue(serde_json::json!({"url": "noop://bench"})).await?;
    assert_eq!(meta(id).await?["sink"], "noop");
    // RFC 3339 as the API takes it; unix 0 is now
    let id = enqueue(serde_json::json!({"url":"noop://x", "at": "2100-01-01T00:00:00Z"})).await?;
    let at: i64 =
        sqlx::query_scalar("SELECT extract(epoch FROM at)::bigint FROM scheduled WHERE id = $1")
            .bind(id)
            .fetch_one(&pool)
            .await?;
    assert_eq!(at, 4_102_444_800);
    enqueue(serde_json::json!({"url":"noop://x", "at": 0})).await?;
    // no timeout is stored
    let id = enqueue(serde_json::json!({"url":"noop://x"})).await?;
    let meta: serde_json::Value = sqlx::query_scalar("SELECT meta FROM jobs WHERE id = $1")
        .bind(id)
        .fetch_one(&pool)
        .await?;
    assert_eq!(meta["sink"], "noop");
    assert!(meta.get("timeout").is_none(), "{meta}");
    // `at` and `delay` are exclusive
    assert!(
        enqueue(serde_json::json!({"url":"noop://x", "delay": 5, "at": 1}))
            .await
            .is_err()
    );
    // `at` far ahead: the job waits
    let id = enqueue(serde_json::json!({"url":"noop://x", "at": 4_102_444_800_i64})).await?;
    let waiting: bool =
        sqlx::query_scalar("SELECT at > now() + interval '1 day' FROM scheduled WHERE id = $1")
            .bind(id)
            .fetch_one(&pool)
            .await?;
    assert!(waiting);
    Ok(())
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn enqueue_v1_body_is_a_string_or_json(pool: PgPool) -> anyhow::Result<()> {
    let stored = |body: serde_json::Value| {
        let pool = pool.clone();
        async move {
            let job = serde_json::json!({"url": "http://localhost/x", "body": body});
            let id: i64 = sqlx::query_scalar("SELECT enqueue_v1($1::jsonb)")
                .bind(job.to_string())
                .fetch_one(&pool)
                .await?;
            sqlx::query_as::<_, (Option<Vec<u8>>, Option<serde_json::Value>)>(
                "SELECT body, headers FROM jobs WHERE id = $1",
            )
            .bind(id)
            .fetch_one(&pool)
            .await
        }
    };
    let (body, headers) = stored(serde_json::json!("key=value&to=me")).await?;
    assert_eq!(body.as_deref(), Some(&b"key=value&to=me"[..]));
    assert!(headers.is_none(), "{headers:?}");
    let (body, headers) = stored(serde_json::json!({"key": "value", "to": "me"})).await?;
    let json: serde_json::Value = serde_json::from_slice(&body.expect("body"))?;
    assert_eq!(json, serde_json::json!({"key": "value", "to": "me"}));
    assert_eq!(
        headers,
        Some(serde_json::json!({"content-type": "application/json"}))
    );
    Ok(())
}
