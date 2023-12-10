use crate::features::schedules::JobSchedule;
use crate::models::{Error, JobEntry};
use crate::models::{JobCreate, JobProtocol, JobRow};
use futures::stream::BoxStream;
use sqlx::{types::Json, Pool, Postgres};

pub async fn enqueue(pool: &Pool<Postgres>, job: JobCreate) -> Result<(i64, Option<String>), Error> {
    const SQL_ENQUEUE: &str = "WITH a AS (
        INSERT INTO jobs(protocol, meta, headers, body) VALUES ($1, $2, $3, $4) RETURNING id
    )
    INSERT INTO enqueued SELECT id FROM a RETURNING id";
    const SQL_SCHEDULE: &str = "WITH a AS (
        INSERT INTO jobs(protocol, meta, headers, body) VALUES ($1, $2, $3, $4) RETURNING id
    )
    INSERT INTO scheduled SELECT id, $5 as at FROM a RETURNING id";

    const SQL_SCHEDULE_S: &str = "
    WITH a AS (
        INSERT INTO jobs(protocol, meta, headers, body, schedule_id) VALUES ($1, $2, $3, $4, $6) RETURNING id
    ), b AS (
        INSERT INTO schedules SELECT $6 as schedule_id, $7 as schedule, id as next_id, $5 as next_at FROM a RETURNING next_id
    ) 
    INSERT INTO scheduled SELECT id, $5 as at FROM a RETURNING id
    ";


    let meta = job.meta;
    let headers = job.headers;
    let body: Option<&[u8]> = match job.body.is_empty() {
        true => None,
        false => Some(job.body.as_ref()),
    };
    let protocol = match &meta.protocol {
        JobProtocol::Http(_) => "http",
        _ => "none",
    };
    if let Some(schedule) = job.schedule {
        let schedule_id = ulid::Ulid::new().to_string();
        let at = schedule.next(JobSchedule::now_secs());
        if let None = at {
            return  Err(Error::InvalidParams("schedule"));
        }

        let job_id =  sqlx::query_scalar::<_, i64>(SQL_SCHEDULE_S)
            .bind(protocol)
            .bind(Json(meta))
            .bind(Json(headers))
            .bind(body)
            .bind(at)
            .bind(&schedule_id)
            .bind(schedule.to_string())
            .fetch_one(pool)
            .await?;
        return Ok((job_id, Some(schedule_id)));
    }

    let job_id = match job.at {
        Some(at) => {
            sqlx::query_scalar::<_, i64>(SQL_SCHEDULE)
                .bind(protocol)
                .bind(Json(meta))
                .bind(Json(headers))
                .bind(body)
                .bind(at)
                .fetch_one(pool)
                .await?
        }
        None => {
            sqlx::query_scalar::<_, i64>(SQL_ENQUEUE)
                .bind(protocol)
                .bind(Json(meta))
                .bind(Json(headers))
                .bind(body)
                .fetch_one(pool)
                .await?
        }
    };
    Ok((job_id, None))
}

pub async fn get_by_id(pool: &Pool<Postgres>, job_id: i64) -> Result<Option<JobRow>, Error> {
    const SQL: &str = "SELECT * FROM jobs WHERE id = $1";
    let job = sqlx::query_as::<_, JobRow>(SQL)
        .bind(job_id)
        .fetch_optional(pool)
        .await?;
    Ok(job)
}

pub async fn delete(pool: &Pool<Postgres>, job_id: i64) -> Result<u64, Error> {
    const SQL: &str = "DELETE FROM jobs WHERE id = $1";
    let res = sqlx::query(SQL).bind(job_id).execute(pool).await?;
    Ok(res.rows_affected())
}

pub async fn enqueue_scheduled(pool: &Pool<Postgres>) -> Result<u64, Error> {
    const SQL: &str = "WITH b AS (
        WITH a AS (
            SELECT id, retry FROM scheduled WHERE at <= extract(epoch from now())::bigint ORDER BY retry, id LIMIT $1 FOR UPDATE SKIP LOCKED
        )
        INSERT INTO enqueued SELECT id, retry FROM a RETURNING id
    )
    DELETE FROM scheduled WHERE id = ANY(SELECT id FROM b)";
    let res = sqlx::query(SQL).bind(1000).execute(pool).await?;
    Ok(res.rows_affected())
}



pub async fn unlock(pool: &Pool<Postgres>, job_id: i64) -> Result<u64, Error> {
    const SQL: &str = "UPDATE enqueued SET instance_id = null, lock_at = null, retry = retry + 1 WHERE id = $1 RETURNING id, retry";
    let res = sqlx::query(SQL).bind(job_id).execute(pool).await?;
    Ok(res.rows_affected())
}

pub async fn retry(pool: &Pool<Postgres>, job_id: i64, at: i64) -> Result<u64, Error> {
    const SQL: &str = "WITH a AS (
        DELETE FROM enqueued WHERE id = $1 RETURNING id, retry, instance_id
    )
    INSERT INTO scheduled SELECT id, $2 as at, (retry + 1) as retry FROM a RETURNING id";
    let res = sqlx::query(SQL)
        .bind(job_id)
        .bind(at)
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}

#[allow(dead_code, unused_variables)]
pub async fn assign_enqueued(
    pool: &Pool<Postgres>,
    instance_id: &str,
    prefetch: i32,
) -> Result<i64, Error> {
    const SQL: &str = "WITH b AS (
        WITH a AS (
            SELECT id, retry FROM enqueued ORDER BY retry, id LIMIT $1 FOR UPDATE SKIP LOCKED
        )
        INSERT INTO assigned SELECT id, retry, $2 as instance_id FROM a RETURNING id
    )
    DELETE FROM enqueued WHERE id = ANY(SELECT id FROM b)";
    let res = sqlx::query_as::<_, (i64, i32)>(SQL)
        .bind(prefetch)
        .bind(instance_id)
        .fetch(pool);
    Ok(0)
}

#[allow(dead_code, unused_variables)]
pub fn fetch_enqueued<'a>(
    pool: &'a Pool<Postgres>,
    instance_id: &'a str,
    prefetch: i32,
) -> BoxStream<'a, Result<JobEntry, sqlx::Error>> {
    const SQL: &str = "WITH a AS (
        SELECT id, retry FROM enqueued WHERE lock_at IS NULL ORDER BY retry, id LIMIT $1 FOR UPDATE SKIP LOCKED
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
    )
    UPDATE enqueued SET instance_id = $2, lock_at = now() WHERE id = ANY(SELECT id FROM a) RETURNING id, retry";
    let res = sqlx::query_as::<_, JobEntry>(SQL)
        .bind(1)
        .bind(instance_id)
        .fetch_optional(pool)
        .await;
    res
}
