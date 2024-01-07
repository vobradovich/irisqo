use crate::features::schedules::JobSchedule;
use crate::models::{Error, JobCreateRow, JobEntry};
use crate::models::{JobCreate, JobRow};
use futures::stream::BoxStream;
use sqlx::{types::Json, Pool, Postgres};

pub async fn create(
    pool: &Pool<Postgres>,
    job: JobCreate,
    instance_id: &str,
) -> Result<JobCreateRow, Error> {
    let external_id = &job.external_id.clone();
    let res = if job.schedule.is_some() {
        create_with_schedule(pool, job, instance_id).await
    } else if job.at.is_some() {
        create_at(pool, job, instance_id).await
    } else {
        create_enqueue(pool, job, instance_id).await
    };
    // Check existing Job for idempotency
    if let Err(Error::DbError(sqlx::Error::Database(ref err))) = res {
        if err.code().unwrap_or_default() == "23505" {
            if let Some(external_id) = external_id {
                let existed_job = get_id_by_external_id(pool, external_id).await?;
                if let Some(job) = existed_job {
                    return Ok(job);
                }
            }
        }
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

    let body: Option<&[u8]> = match job.body.is_empty() {
        true => None,
        false => Some(job.body.as_ref()),
    };
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

    let at = job.at.unwrap();

    let body: Option<&[u8]> = match job.body.is_empty() {
        true => None,
        false => Some(job.body.as_ref()),
    };

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
    instance_id: &str,
) -> Result<JobCreateRow, Error> {
    const SQL: &str = "
    WITH a AS (
        INSERT INTO jobs(meta, headers, body, schedule_id, external_id) VALUES ($1, $2, $3, $4, $6) RETURNING id
    ), b AS (
        INSERT INTO schedules SELECT $6 as schedule_id, $7 as schedule, id as next_id, $5 as next_at FROM a RETURNING next_id
    ), hist AS (
        INSERT INTO history SELECT id, 0 as retry, $8 as instance_id, now() as at, 'scheduled'::history_status as status FROM a RETURNING id
    )
    INSERT INTO scheduled SELECT id, $5 as at FROM a RETURNING id
    ";

    let schedule = job.schedule.unwrap();

    let body: Option<&[u8]> = match job.body.is_empty() {
        true => None,
        false => Some(job.body.as_ref()),
    };

    let after = job.at.unwrap_or_else(|| JobSchedule::now_secs());
    let at = schedule.next(after, job.until);
    if let None = at {
        return Err(Error::InvalidParams("schedule"));
    }
    let schedule_id = ulid::Ulid::new().to_string();

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

pub async fn clone_schedule_at(
    pool: &Pool<Postgres>,
    job_id: i64,
    at: i64,
    instance_id: &str,
) -> Result<i64, Error> {
    const SQL: &str = "
    WITH a AS (
        INSERT INTO jobs(meta, headers, body, schedule_id)
        SELECT meta, headers, body, schedule_id
        FROM jobs
        WHERE id = $1
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
        .fetch_one(pool)
        .await?;
    return Ok(job_id);
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

pub async fn enqueue_scheduled(pool: &Pool<Postgres>, instance_id: &str) -> Result<u64, Error> {
    const SQL: &str = "
    WITH a AS (
        SELECT id, retry FROM scheduled WHERE at <= extract(epoch from now())::bigint ORDER BY retry, id LIMIT $1 FOR UPDATE SKIP LOCKED
    ), b AS (
        INSERT INTO enqueued SELECT id, retry FROM a RETURNING id
    ), hist AS (
        INSERT INTO history SELECT id, retry, $2 as instance_id, now() as at, 'enqueued'::history_status as status FROM a RETURNING id
    )
    DELETE FROM scheduled WHERE id = ANY(SELECT id FROM b)";
    let res = sqlx::query(SQL)
        .bind(1000)
        .bind(instance_id)
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}

pub async fn unlock(pool: &Pool<Postgres>, job_id: i64, instance_id: &str) -> Result<u64, Error> {
    const SQL: &str = "
    WITH a AS (
        UPDATE enqueued SET instance_id = null, lock_at = null, retry = retry + 1 WHERE id = $1 RETURNING id, retry
    )
    INSERT INTO history SELECT id, retry, $2 as instance_id, now() as at, 'retried'::history_status as status FROM a RETURNING id";
    let res = sqlx::query(SQL)
        .bind(job_id)
        .bind(instance_id)
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}

pub async fn retry(pool: &Pool<Postgres>, job_id: i64, at: i64) -> Result<u64, Error> {
    const SQL: &str = "
    WITH a AS (
        DELETE FROM enqueued WHERE id = $1 RETURNING id, retry, instance_id
    ), hist AS (
        INSERT INTO history SELECT id, (retry + 1) as retry, instance_id, now() as at, 'retried'::history_status as status FROM a RETURNING id
    )
    INSERT INTO scheduled SELECT id, $2 as at, (retry + 1) as retry FROM a RETURNING id";
    let res = sqlx::query(SQL).bind(job_id).bind(at).execute(pool).await?;
    Ok(res.rows_affected())
}

#[allow(dead_code, unused_variables)]
pub fn fetch_enqueued<'a>(
    pool: &'a Pool<Postgres>,
    instance_id: &'a str,
    prefetch: i32,
) -> BoxStream<'a, Result<JobEntry, sqlx::Error>> {
    const SQL: &str = "WITH a AS (
        SELECT id, retry FROM enqueued WHERE lock_at IS NULL ORDER BY retry, id LIMIT $1 FOR UPDATE SKIP LOCKED
    ), hist AS (
        INSERT INTO history SELECT id, retry, $2 as instance_id, now() as at, 'assigned'::history_status as status FROM a RETURNING id
    )
    UPDATE enqueued SET instance_id = $2, lock_at = now() WHERE id = ANY(SELECT id FROM a) RETURNING id, retry";
    let res = sqlx::query_as::<_, JobEntry>(SQL)
        .bind(prefetch)
        .bind(instance_id)
        .fetch(pool);
    res
}

pub async fn fetch_optional(
    pool: &Pool<Postgres>,
    instance_id: &str,
) -> Result<Option<JobEntry>, sqlx::Error> {
    const SQL: &str = "WITH a AS (
        SELECT id, retry FROM enqueued WHERE lock_at IS NULL ORDER BY retry, id LIMIT $1 FOR UPDATE SKIP LOCKED
    ), hist AS (
        INSERT INTO history SELECT id, retry, $2 as instance_id, now() as at, 'assigned'::history_status as status FROM a RETURNING id
    )
    UPDATE enqueued SET instance_id = $2, lock_at = now() WHERE id = ANY(SELECT id FROM a) RETURNING id, retry";
    let res = sqlx::query_as::<_, JobEntry>(SQL)
        .bind(1)
        .bind(instance_id)
        .fetch_optional(pool)
        .await;
    res
}
