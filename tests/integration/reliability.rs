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
    sqlx::query("UPDATE claimed SET at = now() - interval '1 hour'")
        .execute(&pool)
        .await?;
    assert_eq!(
        db::jobqueue::reap_expired_locks(&pool, lease, &state.instance_id, 100).await?,
        1
    );
    let (retry, locked): (i32, bool) = sqlx::query_as(
        "SELECT retry, EXISTS (SELECT 1 FROM claimed WHERE id = $1) FROM enqueued WHERE id = $1",
    )
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
        "INSERT INTO instances(instance_id, roles, version, seen_at) VALUES ('old', '{worker}', '1', now() - interval '1 hour')",
    )
    .execute(&pool)
    .await?;
    sqlx::query("WITH a AS (DELETE FROM enqueued RETURNING id, retry) INSERT INTO claimed(id, retry, instance_id) SELECT id, retry, 'old' FROM a")
        .execute(&pool)
        .await?;

    let killed =
        db::instances::kill_expired(&pool, Duration::from_secs(30), &state.instance_id).await?;
    assert_eq!(killed, 1);
    assert_eq!(
        common::count(&pool, "SELECT count(*) FROM enqueued WHERE retry = 1").await,
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
        common::count(&pool, "SELECT count(*) FROM enqueued WHERE retry = 0").await,
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
        "SELECT retry, (extract(epoch from at) - extract(epoch from now()))::bigint FROM scheduled WHERE id = $1",
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
    // retries at once, but the server asks to wait 2 minutes
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
    assert_eq!(timeout, i64::from(state.worker_options.max_timeout));
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

async fn enqueue_in_queue(pool: &PgPool, queue: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT enqueue_v1(jsonb_build_object('sink', 'http', 'url', 'http://localhost/x', 'queue', $1::text))",
    )
    .bind(queue)
    .fetch_one(pool)
    .await
    .expect("enqueue_v1")
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn an_unreadable_job_fails_alone(pool: PgPool) -> anyhow::Result<()> {
    let state = common::state(pool.clone());
    let good = enqueue_in_queue(&pool, "other").await;
    let bad: i64 = sqlx::query_scalar(
        "SELECT enqueue_v1('{\"sink\": \"http\", \"url\": \"http://localhost/x\"}')",
    )
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
async fn a_delayed_retry_keeps_its_place_in_an_ordered_queue(pool: PgPool) -> anyhow::Result<()> {
    let state = common::state(pool.clone());
    let a = enqueue_in_queue(&pool, "q").await;
    enqueue_in_queue(&pool, "q").await;
    let jobs = common::fetch(&state, 10).await;
    let ids: Vec<i64> = jobs.iter().map(|j| j.job.id).collect();
    assert_eq!(ids, [a], "one at a time");

    let entry = irisqo::models::JobEntry { id: a, retry: 0 };
    let at = chrono::Utc::now() + chrono::Duration::seconds(60);
    db::jobqueue::retry(&pool, entry, at, &state.instance_id, "again").await?;
    let next = common::fetch(&state, 10).await;
    assert!(next.is_empty(), "the next job waits for the retry");
    Ok(())
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn a_queue_gives_its_jobs_in_id_order(pool: PgPool) -> anyhow::Result<()> {
    let state = common::state(pool.clone());
    sqlx::query("INSERT INTO queues(queue_id, parallelism) VALUES ('q', 2)")
        .execute(&pool)
        .await?;
    let older = enqueue_in_queue(&pool, "q").await;
    enqueue_in_queue(&pool, "q").await;
    // the older job was retried: still first in its queue
    sqlx::query("UPDATE enqueued SET retry = 1 WHERE id = $1")
        .bind(older)
        .execute(&pool)
        .await?;
    let jobs = common::fetch(&state, 1).await;
    let ids: Vec<i64> = jobs.iter().map(|j| j.job.id).collect();
    assert_eq!(ids, [older]);
    Ok(())
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn enqueue_v1_priority_is_a_smallint_and_not_for_a_queue(pool: PgPool) -> anyhow::Result<()> {
    for (extra, message) in [
        (r#""priority": 99999"#, "priority must be from"),
        (r#""queue": "q", "priority": 1"#, "without a queue only"),
    ] {
        let job = format!(r#"{{"sink": "noop", {extra}}}"#);
        let err = sqlx::query_scalar::<_, i64>("SELECT enqueue_v1($1::jsonb)")
            .bind(job)
            .fetch_one(&pool)
            .await
            .expect_err("rejected");
        assert!(err.to_string().contains(message), "{err}");
    }
    // a zero priority is no priority
    sqlx::query_scalar::<_, i64>(
        r#"SELECT enqueue_v1('{"sink": "noop", "queue": "q", "priority": 0}')"#,
    )
    .fetch_one(&pool)
    .await?;
    Ok(())
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn enqueue_v1_checks_the_form_of_key_and_queue(pool: PgPool) -> anyhow::Result<()> {
    for (field, bad) in [("key", "two\nlines"), ("queue", "no spaces")] {
        let job = serde_json::json!({ "sink": "http", "url": "http://localhost/x", field: bad })
            .to_string();
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
        "SELECT enqueue_v1('{\"sink\": \"http\", \"url\": \"http://localhost/x\", \"key\": \"Order 1/abc=\", \"queue\": \"q:1\"}')",
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
        enqueue(json!({"sink": "http", "url": url, "retry": "fixed", "retry_count": 3})).await?,
        Some(JobRetry::Fixed {
            retry_count: 3,
            retry_delay: 0
        })
    );
    assert_eq!(
        enqueue(
            json!({"sink": "http", "url": url, "retry": "exponential", "retry_count": 5,
            "retry_delay": 2, "retry_max_delay": 60})
        )
        .await?,
        Some(JobRetry::Exponential {
            retry_count: 5,
            retry_delay: 2,
            retry_max_delay: Some(60)
        })
    );
    assert_eq!(enqueue(json!({"sink": "http", "url": url })).await?, None);

    for (job, error) in [
        (
            json!({"sink": "http", "url": url, "retry": "fixd", "retry_count": 3}),
            "retry must be",
        ),
        (
            json!({"sink": "http", "url": url, "retry": "exponential", "retry_count": 3}),
            "retry_delay must be",
        ),
        (
            json!({"sink": "http", "url": url, "retry": "fibonacci", "retry_count": 3, "retry_delay": 1,
                "retry_max_delay": "x"}),
            "retry_max_delay must be",
        ),
    ] {
        let err = enqueue(job).await.expect_err("rejected");
        assert!(err.to_string().contains(error), "{err}");
    }
    Ok(())
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn a_released_or_requeued_job_keeps_its_priority_and_queue(
    pool: PgPool,
) -> anyhow::Result<()> {
    let state = common::state(pool.clone());
    let id = &state.instance_id;
    let urgent: i64 = sqlx::query_scalar(
        "SELECT enqueue_v1('{\"sink\": \"http\", \"url\": \"http://localhost/x\", \"priority\": 5}')",
    )
    .fetch_one(&pool)
    .await?;
    let queued = enqueue_in_queue(&pool, "q").await;
    let sql = "SELECT id, queue, priority FROM enqueued ORDER BY id";
    let want = vec![(urgent, None, 5_i16), (queued, Some("q".to_owned()), 0)];

    // claim, then release on shutdown
    assert_eq!(common::fetch(&state, 10).await.len(), 2);
    db::instances::kill(&pool, id).await?;
    assert_eq!(
        sqlx::query_as::<_, (i64, Option<String>, i16)>(sql)
            .fetch_all(&pool)
            .await?,
        want
    );

    // claim, then an immediate retry of one job
    let jobs = common::fetch(&state, 10).await;
    let entry = irisqo::models::JobEntry {
        id: urgent,
        retry: jobs[0].retry,
    };
    assert_eq!(db::jobqueue::unlock(&pool, entry, id, "again").await?, 1);
    let rows = sqlx::query_as::<_, (i64, Option<String>, i16)>(sql)
        .fetch_all(&pool)
        .await?;
    assert_eq!(rows, vec![want[0].clone()]);

    // processed, then run once more by a user
    sqlx::query("WITH a AS (DELETE FROM claimed RETURNING id, retry) INSERT INTO processed(id, retry, instance_id, status) SELECT id, retry, 'x', 'failed' FROM a")
        .execute(&pool)
        .await?;
    for job in [urgent, queued] {
        db::jobqueue::requeue_job(&pool, job, id).await?;
    }
    let rows = sqlx::query_as::<_, (i64, Option<String>, i16)>(sql)
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
    let database: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(&pool)
        .await?;
    let mut url = url::Url::parse(&std::env::var("DATABASE_URL")?)?;
    url.set_path(&database);
    let options = irisqo::models::Options::from_vec(vec![
        "--db-url".into(),
        url.to_string().into(),
        "--db-schema".into(),
        "mine".into(),
        "--env".into(),
        "development".into(),
    ])
    .expect("options");
    let state = irisqo::AppState::new(&options).await;

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
    let id: i64 = sqlx::query_scalar(
        "SELECT enqueue_v1('{\"sink\": \"http\", \"url\": \"http://localhost/x\"}')",
    )
    .fetch_one(&state.pool)
    .await?;
    let jobs = common::fetch(&state, 10).await;
    assert_eq!(jobs.iter().map(|j| j.job.id).collect::<Vec<_>>(), [id]);
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
                    sqlx::query("SELECT enqueue_v1('{\"sink\": \"http\", \"url\": \"http://localhost/x\"}')")
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
async fn jobs_without_a_queue_come_first_and_the_queues_get_the_rest(
    pool: PgPool,
) -> anyhow::Result<()> {
    let state = common::state(pool.clone());
    let queued = enqueue_in_queue(&pool, "q").await;
    let plain = common::insert_enqueued(&pool, 2).await;
    let urgent: i64 = sqlx::query_scalar(
        "SELECT enqueue_v1('{\"sink\": \"http\", \"url\": \"http://localhost/x\", \"priority\": 5}')",
    )
    .fetch_one(&pool)
    .await?;

    // the batch is full of jobs without a queue: by priority, then by id; the queue waits
    let ids =
        |jobs: Vec<irisqo::models::JobWithRetry>| jobs.iter().map(|j| j.job.id).collect::<Vec<_>>();
    assert_eq!(
        ids(common::fetch(&state, 3).await),
        [urgent, plain[0], plain[1]]
    );
    assert_eq!(
        common::count(&pool, "SELECT count(*) FROM claimed WHERE queue = 'q'").await,
        0
    );

    // the queue gets the room that is left
    let more = enqueue_in_queue(&pool, "other").await;
    assert_eq!(ids(common::fetch(&state, 5).await), [queued, more]);
    Ok(())
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn a_held_head_of_an_ordered_queue_does_not_cost_the_backlog(
    pool: PgPool,
) -> anyhow::Result<()> {
    let state = common::state(pool.clone());
    sqlx::query("INSERT INTO queues(queue_id, parallelism) VALUES ('q', 8)")
        .execute(&pool)
        .await?;
    sqlx::query(
        "WITH a AS (INSERT INTO jobs(meta) SELECT jsonb_build_object('retry', 'none', 'timeout', 10000, 'sink', 'noop') FROM generate_series(1, 100000) RETURNING id)
         INSERT INTO enqueued(id, queue) SELECT id, 'q' FROM a",
    )
    .execute(&pool)
    .await?;
    sqlx::query("ANALYZE enqueued").execute(&pool).await?;
    // the head is a retry with a delay: it holds its place, the backlog waits for it
    sqlx::query("UPDATE enqueued SET at = now() + interval '1 hour' WHERE id = (SELECT min(id) FROM enqueued)")
        .execute(&pool)
        .await?;

    let started = std::time::Instant::now();
    assert!(common::fetch(&state, 64).await.is_empty());
    assert!(
        started.elapsed() < Duration::from_millis(100),
        "the claim read the backlog: {:?}",
        started.elapsed()
    );

    // the head is due: the queue gives its jobs again (parallelism 8)
    sqlx::query("UPDATE enqueued SET at = now() WHERE id = (SELECT min(id) FROM enqueued)")
        .execute(&pool)
        .await?;
    assert_eq!(common::fetch(&state, 64).await.len(), 8);
    Ok(())
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn an_ordered_queue_gives_the_rows_before_a_held_one(pool: PgPool) -> anyhow::Result<()> {
    let state = common::state(pool.clone());
    sqlx::query("INSERT INTO queues(queue_id, parallelism) VALUES ('q', 8)")
        .execute(&pool)
        .await?;
    let mut jobs = Vec::new();
    for _ in 0..4 {
        jobs.push(enqueue_in_queue(&pool, "q").await);
    }
    sqlx::query("UPDATE enqueued SET at = now() + interval '1 hour' WHERE id = $1")
        .bind(jobs[2])
        .execute(&pool)
        .await?;
    let got: Vec<i64> = common::fetch(&state, 10)
        .await
        .iter()
        .map(|j| j.job.id)
        .collect();
    assert_eq!(
        got,
        [jobs[0], jobs[1]],
        "the rows after the held one wait for it"
    );

    // an unordered queue skips the held row
    sqlx::query("UPDATE queues SET ordered = false WHERE queue_id = 'q'")
        .execute(&pool)
        .await?;
    let got: Vec<i64> = common::fetch(&state, 10)
        .await
        .iter()
        .map(|j| j.job.id)
        .collect();
    assert_eq!(got, [jobs[3]]);
    Ok(())
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn enqueue_v1_needs_a_sink_and_at_excludes_delay(pool: PgPool) -> anyhow::Result<()> {
    let enqueue = |job: serde_json::Value| {
        sqlx::query_scalar::<_, i64>("SELECT enqueue_v1($1::jsonb)")
            .bind(job.to_string())
            .fetch_one(&pool)
    };
    for job in [
        serde_json::json!({"url": "http://localhost/x"}),
        serde_json::json!({"sink": "smtp"}),
        serde_json::json!({"sink": "http"}),
    ] {
        assert!(enqueue(job).await.is_err());
    }
    // noop needs no url; no timeout is stored
    let id = enqueue(serde_json::json!({"sink": "noop"})).await?;
    let meta: serde_json::Value = sqlx::query_scalar("SELECT meta FROM jobs WHERE id = $1")
        .bind(id)
        .fetch_one(&pool)
        .await?;
    assert_eq!(meta["sink"], "noop");
    assert!(meta.get("timeout").is_none(), "{meta}");
    // `at` and `delay` are exclusive
    assert!(
        enqueue(serde_json::json!({"sink": "noop", "delay": 5, "at": 1}))
            .await
            .is_err()
    );
    // `at` far ahead: the job waits
    let id = enqueue(serde_json::json!({"sink": "noop", "at": 4_102_444_800_i64})).await?;
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
            let job =
                serde_json::json!({"sink": "http", "url": "http://localhost/x", "body": body});
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
