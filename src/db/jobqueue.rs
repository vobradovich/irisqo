use std::time::Duration;

use crate::features;
use crate::features::results::{self, JobResult};
use crate::models::{Error, JobCreateRow, JobEntry, JobWithRetry};
use crate::models::{JobCreate, JobRow};
use chrono::{DateTime, Utc};
use sqlx::{FromRow, Pool, Postgres, Row, types::Json};

pub async fn create(
    pool: &Pool<Postgres>,
    mut job: JobCreate,
    instance_id: &str,
) -> Result<JobCreateRow, Error> {
    if job.schedule.is_some() {
        // the same declaration as `PUT /schedules/{id}`: the key is the id of the schedule,
        // a new unique one when there is none
        job.key.get_or_insert_with(crate::models::new_ulid);
        return features::schedules::db::upsert(pool, job, instance_id).await;
    }
    let key = job.key.clone();
    let res = if let Some(at) = job.at {
        create_at(pool, job, at, instance_id).await
    } else {
        create_enqueue(pool, job, instance_id).await
    };
    // Check existing Job for idempotency
    if let Err(Error::DbError(sqlx::Error::Database(ref err))) = res
        && err.code().unwrap_or_default() == "23505"
        && let Some(key) = key
        && let Some(job) = get_id_by_key(pool, &key).await?
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
        INSERT INTO jobs(meta, headers, body, key) VALUES ($1, $2, $3, $4) RETURNING id
    ), hist AS (
        INSERT INTO history(id, retry, kind, instance_id) SELECT id, 0, 'enqueued', $5 FROM a
    )
    INSERT INTO enqueued(id) SELECT id FROM a RETURNING id";

    let body = job.body_bytes();
    let job_id = sqlx::query_scalar::<_, i64>(SQL)
        .bind(Json(&job.meta))
        .bind(Json(&job.headers))
        .bind(body)
        .bind(&job.key)
        .bind(instance_id)
        .fetch_one(pool)
        .await?;
    Ok(JobCreateRow {
        id: job_id,
        schedule_id: None,
        key: job.key,
    })
}

async fn create_at(
    pool: &Pool<Postgres>,
    job: JobCreate,
    at: DateTime<Utc>,
    instance_id: &str,
) -> Result<JobCreateRow, Error> {
    const SQL: &str = "
    WITH a AS (
        INSERT INTO jobs(meta, headers, body, key) VALUES ($1, $2, $3, $4) RETURNING id
    ), hist AS (
        INSERT INTO history(id, retry, kind, instance_id) SELECT id, 0, 'scheduled', $6 FROM a
    )
    INSERT INTO scheduled(id, at) SELECT id, $5::timestamptz FROM a RETURNING id
    ";

    let body = job.body_bytes();

    let job_id = sqlx::query_scalar::<_, i64>(SQL)
        .bind(Json(&job.meta))
        .bind(Json(&job.headers))
        .bind(body)
        .bind(&job.key)
        .bind(at)
        .bind(instance_id)
        .fetch_one(pool)
        .await?;
    Ok(JobCreateRow {
        id: job_id,
        schedule_id: None,
        key: job.key,
    })
}

pub async fn get_by_id(pool: &Pool<Postgres>, job_id: i64) -> Result<Option<JobRow>, Error> {
    const SQL: &str = "SELECT * FROM jobs WHERE id = $1";
    let job = sqlx::query_as::<_, JobRow>(SQL)
        .bind(job_id)
        .fetch_optional(pool)
        .await?;
    Ok(job)
}

pub async fn get_id_by_key(
    pool: &Pool<Postgres>,
    key: &str,
) -> Result<Option<JobCreateRow>, Error> {
    const SQL: &str = "SELECT id, schedule_id, key FROM jobs WHERE key = $1";
    let job = sqlx::query_as::<_, JobCreateRow>(SQL)
        .bind(key)
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
        DELETE FROM scheduled
        WHERE id IN (SELECT id FROM scheduled WHERE at <= now() ORDER BY at, id LIMIT $1 FOR UPDATE SKIP LOCKED)
        RETURNING id, retry, at, queue, priority
    ), b AS (
        INSERT INTO enqueued(id, retry, at, queue, priority) SELECT id, retry, at, queue, priority FROM a RETURNING id, retry
    )
    INSERT INTO history(id, retry, kind, instance_id) SELECT id, retry, 'enqueued', $2 FROM b";
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
    message: &str,
) -> Result<u64, Error> {
    const SQL: &str = "
    WITH a AS (
        DELETE FROM claimed WHERE id = $1 AND instance_id = $2 AND retry = $3 RETURNING id, retry, queue, priority
    ), b AS (
        INSERT INTO enqueued(id, retry, queue, priority) SELECT id, retry + 1, queue, priority FROM a RETURNING id, retry
    )
    INSERT INTO history(id, retry, kind, instance_id, message) SELECT id, retry, 'retried', $2, $4 FROM b";
    let res = sqlx::query(SQL)
        .bind(entry.id)
        .bind(instance_id)
        .bind(entry.retry)
        .bind(message)
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}

/// Moves a job to `scheduled` for a retry at `at`.
/// Only the current attempt (`instance_id` + `retry`) may do it: returns 0 otherwise.
pub async fn retry(
    pool: &Pool<Postgres>,
    entry: JobEntry,
    at: DateTime<Utc>,
    instance_id: &str,
    message: &str,
) -> Result<u64, Error> {
    const SQL: &str = "
    WITH a AS (
        DELETE FROM claimed WHERE id = $1 AND instance_id = $3 AND retry = $4
        RETURNING id, retry + 1 AS retry, queue, priority, instance_id
    ), r AS (
        -- in an ordered queue the retry holds its place: later jobs wait for it in `enqueued`
        SELECT a.*, (a.queue IS NOT NULL AND coalesce(q.ordered, true)) AS held
        FROM a LEFT JOIN queues q ON q.queue_id = a.queue
    ), hist AS (
        INSERT INTO history(id, retry, kind, instance_id, message)
        SELECT id, retry, 'retried', instance_id, $5 FROM r
    ), held AS (
        INSERT INTO enqueued(id, retry, at, queue, priority) SELECT id, retry, $2::timestamptz, queue, priority FROM r WHERE held
    )
    INSERT INTO scheduled(id, at, retry, queue, priority) SELECT id, $2::timestamptz, retry, queue, priority FROM r WHERE NOT held";
    let res = sqlx::query(SQL)
        .bind(entry.id)
        .bind(at)
        .bind(instance_id)
        .bind(entry.retry)
        .bind(message)
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
        DELETE FROM claimed
        WHERE id IN (SELECT id FROM claimed WHERE at < now() - $1 ORDER BY at LIMIT $3 FOR UPDATE SKIP LOCKED)
        RETURNING id, retry, queue, priority, instance_id
    ), b AS (
        INSERT INTO enqueued(id, retry, queue, priority) SELECT id, retry + 1, queue, priority FROM a
    )
    INSERT INTO history(id, retry, kind, instance_id, message)
    SELECT id, retry + 1, 'retried', $2, 'lock expired: ' || instance_id FROM a";
    let res = sqlx::query(SQL)
        .bind(lease)
        .bind(instance_id)
        .bind(limit)
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}

/// Claims up to `prefetch` enqueued jobs for `instance_id` and returns them (`claim_v1`: the
/// jobs without a queue first, the queues get what is left, in one call and one transaction).
///
/// `claim_v1` reads the jobs in the statement that claims them: a join with `jobs` outside
/// would use an older snapshot and miss a job enqueued meanwhile, which would stay claimed and
/// never run. The claim is committed before any job runs: a worker completing a job never
/// waits on the fetch itself.
pub async fn fetch_job_with_retry(
    pool: &Pool<Postgres>,
    instance_id: &str,
    prefetch: i32,
) -> Result<Vec<JobWithRetry>, Error> {
    const SQL: &str = "SELECT * FROM claim_v1($2, $1)";
    let rows = sqlx::query(SQL)
        .bind(prefetch)
        .bind(instance_id)
        .fetch_all(pool)
        .await?;
    let mut jobs = Vec::with_capacity(rows.len());
    for row in rows {
        match JobWithRetry::from_row(&row) {
            Ok(job) => jobs.push(job),
            // an unreadable job (a bad URL, say) fails alone, it must not hold the batch
            Err(err) => {
                let (Ok(id), Ok(retry)) = (row.try_get("id"), row.try_get("retry")) else {
                    tracing::error!("unreadable job without an id: {err:?}");
                    continue;
                };
                let entry = JobEntry { id, retry };
                tracing::error!(job_id = id, "unreadable job: {err:?}");
                let result = JobResult::from(Error::from(err));
                // when this write fails too, the job stays claimed (the lock reaper releases it)
                // but the rest of the batch must still run
                _ = results::processed(pool, entry, instance_id, &result)
                    .await
                    .inspect_err(|err| {
                        tracing::error!(job_id = id, "result of an unreadable job: {err:?}");
                    });
            }
        }
    }
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
/// A running or processed job is a conflict. A run of a schedule gives its key back: it never
/// ran, and the schedule may need a run of the same time (`{schedule_id}-{secs}`) again.
pub async fn cancel<'e, E>(executor: E, job_id: i64, instance_id: &str) -> Result<u64, Error>
where
    E: sqlx::Executor<'e, Database = Postgres>,
{
    const SQL: &str = "
    WITH s AS (
        DELETE FROM scheduled WHERE id = $1 RETURNING id, retry
    ), e AS (
        DELETE FROM enqueued WHERE id = $1 RETURNING id, retry
    ), c AS (
        SELECT id, retry FROM s UNION ALL SELECT id, retry FROM e
    ), hist AS (
        INSERT INTO history(id, retry, kind, instance_id, message)
        SELECT id, retry, 'cancelled', $2, 'cancelled by a user' FROM c
    ), k AS (
        UPDATE jobs SET key = NULL WHERE id IN (SELECT id FROM c) AND schedule_id IS NOT NULL
    )
    INSERT INTO processed(id, retry, instance_id, status)
    SELECT id, retry, $2, 'cancelled' FROM c";
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
        INSERT INTO enqueued(id, retry, queue, priority)
        SELECT p.id, p.retry + 1, j.meta->>'queue', coalesce((j.meta->>'priority')::smallint, 0)
        FROM p INNER JOIN jobs AS j ON j.id = p.id
        RETURNING id, retry
    )
    INSERT INTO history(id, retry, kind, instance_id, message)
    SELECT id, retry, 'enqueued', $2, 'retried by a user' FROM e";
    let res = sqlx::query(SQL)
        .bind(job_id)
        .bind(instance_id)
        .execute(pool)
        .await?;
    outcome(pool, res.rows_affected(), job_id).await
}
