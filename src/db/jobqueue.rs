use std::time::Duration;

use crate::features::schedules::JobSchedule;
use crate::models::{Error, JobCreateRow, JobEntry, JobWithRetry};
use crate::models::{JobCreate, JobRow};
use sqlx::{Pool, Postgres, types::Json};

pub async fn create(
    pool: &Pool<Postgres>,
    mut job: JobCreate,
    instance_id: &str,
) -> Result<JobCreateRow, Error> {
    let external_id = &job.external_id.clone();
    let res = if let Some(schedule) = job.schedule.take() {
        create_with_schedule(pool, job, schedule, instance_id).await
    } else if let Some(at) = job.at {
        create_at(pool, job, at, instance_id).await
    } else {
        create_enqueue(pool, job, instance_id).await
    };
    // Check existing Job for idempotency
    if let Err(Error::DbError(sqlx::Error::Database(ref err))) = res
        && err.code().unwrap_or_default() == "23505"
        && let Some(external_id) = external_id
        && let Some(job) = get_id_by_external_id(pool, external_id).await?
    {
        return Ok(job);
    }
    res
}

async fn create_enqueue(
    pool: &Pool<Postgres>,
    job: JobCreate,
    instance_id: &str,
) -> Result<JobCreateRow, Error> {
    const SQL: &str = "
    WITH a AS (
        INSERT INTO jobs(meta, headers, body, external_id) VALUES ($1, $2, $3, $4) RETURNING id
    ), hist AS (
        INSERT INTO history SELECT id, 0 as retry, $5 as instance_id, now() as at, 'enqueued'::history_status as status FROM a RETURNING id
    )
    INSERT INTO enqueued SELECT id FROM a RETURNING id";

    let body: Option<&[u8]> = (!job.body.is_empty()).then_some(job.body.as_ref());
    let job_id = sqlx::query_scalar::<_, i64>(SQL)
        .bind(Json(&job.meta))
        .bind(Json(&job.headers))
        .bind(body)
        .bind(&job.external_id)
        .bind(instance_id)
        .fetch_one(pool)
        .await?;
    Ok(JobCreateRow {
        id: job_id,
        schedule_id: None,
        external_id: job.external_id,
    })
}

async fn create_at(
    pool: &Pool<Postgres>,
    job: JobCreate,
    at: i64,
    instance_id: &str,
) -> Result<JobCreateRow, Error> {
    const SQL: &str = "
    WITH a AS (
        INSERT INTO jobs(meta, headers, body, external_id) VALUES ($1, $2, $3, $4) RETURNING id
    ), hist AS (
        INSERT INTO history SELECT id, 0 as retry, $6 as instance_id, now() as at, 'scheduled'::history_status as status FROM a RETURNING id
    )
    INSERT INTO scheduled SELECT id, $5 as at FROM a RETURNING id
    ";

    let body: Option<&[u8]> = (!job.body.is_empty()).then_some(job.body.as_ref());

    let job_id = sqlx::query_scalar::<_, i64>(SQL)
        .bind(Json(&job.meta))
        .bind(Json(&job.headers))
        .bind(body)
        .bind(&job.external_id)
        .bind(at)
        .bind(instance_id)
        .fetch_one(pool)
        .await?;
    Ok(JobCreateRow {
        id: job_id,
        schedule_id: None,
        external_id: job.external_id,
    })
}

async fn create_with_schedule(
    pool: &Pool<Postgres>,
    job: JobCreate,
    schedule: JobSchedule,
    instance_id: &str,
) -> Result<JobCreateRow, Error> {
    const SQL: &str = "
    WITH a AS (
        INSERT INTO jobs(meta, headers, body, external_id, schedule_id) VALUES ($1, $2, $3, $4, $6) RETURNING id
    ), b AS (
        INSERT INTO schedules SELECT $6 as schedule_id, $7 as schedule, id as next_id, $5 as next_at FROM a RETURNING next_id
    ), hist AS (
        INSERT INTO history SELECT id, 0 as retry, $8 as instance_id, now() as at, 'scheduled'::history_status as status FROM a RETURNING id
    )
    INSERT INTO scheduled SELECT id, $5 as at FROM a RETURNING id
    ";

    let body: Option<&[u8]> = (!job.body.is_empty()).then_some(job.body.as_ref());

    let after = job.at.unwrap_or_else(JobSchedule::now_secs);
    let at = schedule.next(after, job.until);
    // prepare_job checks it first: a schedule must run at least once
    if at.is_none() {
        return Err(Error::invalid(
            "_until",
            "the schedule has no run before it",
        ));
    }
    let schedule_id = ulid::Ulid::generate().to_string();

    let job_id = sqlx::query_scalar::<_, i64>(SQL)
        .bind(Json(&job.meta))
        .bind(Json(&job.headers))
        .bind(body)
        .bind(&job.external_id)
        .bind(at)
        .bind(&schedule_id)
        .bind(schedule.to_string())
        .bind(instance_id)
        .fetch_one(pool)
        .await?;
    Ok(JobCreateRow {
        id: job_id,
        schedule_id: Some(schedule_id),
        external_id: job.external_id,
    })
}

/// Clones the job as the next run of its schedule.
/// Returns `None` when the schedule was deleted or deactivated in the meantime,
/// or when the job is not its latest run.
pub async fn clone_schedule_at<'e, E>(
    executor: E,
    job_id: i64,
    at: i64,
    instance_id: &str,
) -> Result<Option<i64>, Error>
where
    E: sqlx::Executor<'e, Database = Postgres>,
{
    const SQL: &str = "
    WITH a AS (
        INSERT INTO jobs(meta, headers, body, schedule_id)
        SELECT j.meta, j.headers, j.body, j.schedule_id
        FROM jobs AS j
        -- only the latest run continues the schedule (a manually retried old run does not)
        INNER JOIN schedules AS s ON s.schedule_id = j.schedule_id AND NOT s.inactive AND s.next_id = j.id
        WHERE j.id = $1
        RETURNING id, schedule_id
    ), b AS (
        UPDATE schedules
        SET next_id = a.id, next_at = $2
        FROM a
        WHERE schedules.schedule_id = a.schedule_id
    ), hist AS (
        INSERT INTO history SELECT id, 0 as retry, $3 as instance_id, now() as at, 'scheduled'::history_status as status FROM a RETURNING id
    )
    INSERT INTO scheduled SELECT id, $2 as at FROM a RETURNING id
    ";

    let job_id = sqlx::query_scalar::<_, i64>(SQL)
        .bind(job_id)
        .bind(at)
        .bind(instance_id)
        .fetch_optional(executor)
        .await?;
    Ok(job_id)
}

pub async fn get_by_id(pool: &Pool<Postgres>, job_id: i64) -> Result<Option<JobRow>, Error> {
    const SQL: &str = "SELECT * FROM jobs WHERE id = $1";
    let job = sqlx::query_as::<_, JobRow>(SQL)
        .bind(job_id)
        .fetch_optional(pool)
        .await?;
    Ok(job)
}

pub async fn get_id_by_external_id(
    pool: &Pool<Postgres>,
    external_id: &str,
) -> Result<Option<JobCreateRow>, Error> {
    const SQL: &str = "SELECT id, schedule_id, external_id FROM jobs WHERE external_id = $1";
    let job = sqlx::query_as::<_, JobCreateRow>(SQL)
        .bind(external_id)
        .fetch_optional(pool)
        .await?;
    Ok(job)
}

pub async fn delete(pool: &Pool<Postgres>, job_id: i64) -> Result<u64, Error> {
    const SQL: &str = "DELETE FROM jobs WHERE id = $1";
    let res = sqlx::query(SQL).bind(job_id).execute(pool).await?;
    Ok(res.rows_affected())
}

/// Moves up to `limit` due jobs from `scheduled` to `enqueued`.
pub async fn enqueue_scheduled(
    pool: &Pool<Postgres>,
    instance_id: &str,
    limit: i32,
) -> Result<u64, Error> {
    const SQL: &str = "
    WITH a AS (
        SELECT id, retry, at FROM scheduled WHERE at <= extract(epoch from now())::bigint ORDER BY retry, id LIMIT $1 FOR UPDATE SKIP LOCKED
    ), b AS (
        INSERT INTO enqueued(id, retry, due_at) SELECT id, retry, to_timestamp(at) FROM a RETURNING id
    ), hist AS (
        INSERT INTO history SELECT id, retry, $2 as instance_id, now() as at, 'enqueued'::history_status as status FROM a RETURNING id
    )
    DELETE FROM scheduled WHERE id = ANY(SELECT id FROM b)";
    let res = sqlx::query(SQL)
        .bind(limit)
        .bind(instance_id)
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}

/// Puts a job back to the queue for an immediate retry.
/// Only the current attempt (`instance_id` + `retry`) may do it: returns 0 otherwise.
pub async fn unlock(
    pool: &Pool<Postgres>,
    entry: JobEntry,
    instance_id: &str,
) -> Result<u64, Error> {
    const SQL: &str = "
    WITH a AS (
        UPDATE enqueued SET instance_id = null, lock_at = null, retry = retry + 1, due_at = now() WHERE id = $1 AND instance_id = $2 AND retry = $3 RETURNING id, retry
    )
    INSERT INTO history SELECT id, retry, $2 as instance_id, now() as at, 'retried'::history_status as status FROM a RETURNING id";
    let res = sqlx::query(SQL)
        .bind(entry.id)
        .bind(instance_id)
        .bind(entry.retry)
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}

/// Moves a job to `scheduled` for a retry at `at` (unix seconds).
/// Only the current attempt (`instance_id` + `retry`) may do it: returns 0 otherwise.
pub async fn retry(
    pool: &Pool<Postgres>,
    entry: JobEntry,
    at: i64,
    instance_id: &str,
) -> Result<u64, Error> {
    const SQL: &str = "
    WITH a AS (
        DELETE FROM enqueued WHERE id = $1 AND instance_id = $3 AND retry = $4 RETURNING id, retry, instance_id
    ), hist AS (
        INSERT INTO history SELECT id, (retry + 1) as retry, instance_id, now() as at, 'retried'::history_status as status FROM a RETURNING id
    )
    INSERT INTO scheduled SELECT id, $2 as at, (retry + 1) as retry FROM a RETURNING id";
    let res = sqlx::query(SQL)
        .bind(entry.id)
        .bind(at)
        .bind(instance_id)
        .bind(entry.retry)
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}

/// Releases up to `limit` jobs locked for longer than `lease` (`retry + 1`, so a late
/// result of the stuck attempt is rejected), whatever instance holds them.
pub async fn reap_expired_locks(
    pool: &Pool<Postgres>,
    lease: Duration,
    instance_id: &str,
    limit: i32,
) -> Result<u64, Error> {
    const SQL: &str = "
    WITH a AS (
        SELECT id, instance_id FROM enqueued WHERE lock_at < now() - $1 ORDER BY lock_at LIMIT $3 FOR UPDATE SKIP LOCKED
    ), b AS (
        UPDATE enqueued AS e SET instance_id = null, lock_at = null, retry = e.retry + 1, due_at = now()
        FROM a WHERE e.id = a.id
        RETURNING e.id, e.retry, a.instance_id AS locked_by
    )
    INSERT INTO history(id, retry, instance_id, status, message)
    SELECT id, retry, $2, 'retried'::history_status, 'lock expired: ' || locked_by FROM b";
    let res = sqlx::query(SQL)
        .bind(lease)
        .bind(instance_id)
        .bind(limit)
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}

/// Assigns up to `prefetch` enqueued jobs to `instance_id`.
///
/// The rows are fetched in full, so the statement (and its row locks) is committed
/// before any job runs: a worker completing a job never waits on the fetch itself.
pub async fn fetch_job_with_retry(
    pool: &Pool<Postgres>,
    instance_id: &str,
    prefetch: i32,
) -> Result<Vec<JobWithRetry>, Error> {
    const SQL: &str = "
    WITH a AS (
        SELECT id, retry FROM enqueued WHERE lock_at IS NULL ORDER BY retry, id LIMIT $1 FOR UPDATE SKIP LOCKED
    ), b AS (
        UPDATE enqueued SET instance_id = $2, lock_at = now() WHERE id = ANY(SELECT id FROM a) RETURNING id, retry
    ), hist AS (
        INSERT INTO history SELECT id, retry, $2 as instance_id, now() as at, 'assigned'::history_status as status FROM a RETURNING id
    )
    SELECT j.*, a.retry FROM a INNER JOIN jobs as j ON a.id = j.id
    ";
    let jobs = sqlx::query_as::<_, JobWithRetry>(SQL)
        .bind(prefetch)
        .bind(instance_id)
        .fetch_all(pool)
        .await?;
    Ok(jobs)
}

/// What a user action did to a job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionOutcome {
    Done,
    /// The job is not in a state the action applies to (running, already processed...).
    Conflict,
    NotFound,
}

async fn outcome(
    pool: &Pool<Postgres>,
    affected: u64,
    job_id: i64,
) -> Result<ActionOutcome, Error> {
    if affected > 0 {
        return Ok(ActionOutcome::Done);
    }
    let exists: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM jobs WHERE id = $1)")
        .bind(job_id)
        .fetch_one(pool)
        .await?;
    Ok(if exists {
        ActionOutcome::Conflict
    } else {
        ActionOutcome::NotFound
    })
}

/// Cancels a pending job (scheduled, or enqueued and not running): it ends as `cancelled`.
/// A running or processed job is a conflict.
pub async fn cancel<'e, E>(executor: E, job_id: i64, instance_id: &str) -> Result<u64, Error>
where
    E: sqlx::Executor<'e, Database = Postgres>,
{
    const SQL: &str = "
    WITH s AS (
        DELETE FROM scheduled WHERE id = $1 RETURNING id, retry
    ), e AS (
        DELETE FROM enqueued WHERE id = $1 AND lock_at IS NULL RETURNING id, retry
    ), c AS (
        SELECT id, retry FROM s UNION ALL SELECT id, retry FROM e
    ), hist AS (
        INSERT INTO history(id, retry, instance_id, status, message)
        SELECT id, retry, $2, 'cancelled'::history_status, 'cancelled by a user' FROM c
    )
    INSERT INTO processed(id, retry, instance_id, status, meta)
    SELECT id, retry, $2, 'cancelled'::processed_status, '{\"result\": \"cancelled\"}'::jsonb FROM c";
    let res = sqlx::query(SQL)
        .bind(job_id)
        .bind(instance_id)
        .execute(executor)
        .await?;
    Ok(res.rows_affected())
}

pub async fn cancel_job(
    pool: &Pool<Postgres>,
    job_id: i64,
    instance_id: &str,
) -> Result<ActionOutcome, Error> {
    let affected = cancel(pool, job_id, instance_id).await?;
    outcome(pool, affected, job_id).await
}

/// Runs a processed job (completed, failed or cancelled) once more, now.
/// The retry number goes on (it fences the attempts), so the retry policy is not restarted.
pub async fn requeue_job(
    pool: &Pool<Postgres>,
    job_id: i64,
    instance_id: &str,
) -> Result<ActionOutcome, Error> {
    const SQL: &str = "
    WITH p AS (
        DELETE FROM processed WHERE id = $1 RETURNING id, retry
    ), e AS (
        INSERT INTO enqueued(id, retry) SELECT id, retry + 1 FROM p RETURNING id, retry
    )
    INSERT INTO history(id, retry, instance_id, status, message)
    SELECT id, retry, $2, 'enqueued'::history_status, 'retried by a user' FROM e";
    let res = sqlx::query(SQL)
        .bind(job_id)
        .bind(instance_id)
        .execute(pool)
        .await?;
    outcome(pool, res.rows_affected(), job_id).await
}
