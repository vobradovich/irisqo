use crate::models::Error;
use crate::models::{JobCreate, JobProtocol, JobQueueRow, JobRow};
use futures::stream::BoxStream;
use sqlx::{types::Json, Pool, Postgres};

pub async fn enqueue(pool: &Pool<Postgres>, job: JobCreate) -> Result<i64, Error> {
    const SQL_ENQUEUE: &str = "WITH a AS (
        INSERT INTO jobs(protocol, url, meta, headers, body) VALUES ($1, $2, $3, $4, $5) RETURNING id
    )
    INSERT INTO enqueued SELECT id FROM a RETURNING id";
    const SQL_SCHEDULE: &str = "WITH a AS (
        INSERT INTO jobs(protocol, url, meta, headers, body) VALUES ($1, $2, $3, $4, $5) RETURNING id
    )
    INSERT INTO scheduled SELECT id, $6 as at FROM a RETURNING id";

    let meta = job.meta;
    let headers = job.headers;
    let body: Option<&[u8]> = match job.body.is_empty() {
        true => None,
        false => Some(job.body.as_ref()),
    };
    let (p, u) = match &meta.protocol {
        JobProtocol::Http(h) => ("http", h.url.to_string()),
        _ => ("null", String::new()),
    };

    let job_id = match job.at {
        Some(at) => {
            sqlx::query_scalar::<_, i64>(SQL_SCHEDULE)
                .bind(p)
                .bind(&u)
                .bind(Json(meta))
                .bind(Json(headers))
                .bind(body)
                .bind(i64::try_from(at).unwrap())
                .fetch_one(pool)
                .await?
        }
        None => {
            sqlx::query_scalar::<_, i64>(SQL_ENQUEUE)
                .bind(p)
                .bind(&u)
                .bind(Json(meta))
                .bind(Json(headers))
                .bind(body)
                .fetch_one(pool)
                .await?
        }
    };
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

pub async fn succeed(pool: &Pool<Postgres>, job_id: i64) -> Result<u64, Error> {
    const SQL: &str = "WITH a AS (
        DELETE FROM enqueued WHERE id = $1 RETURNING id, retry, instance_id
    )
    INSERT INTO history SELECT id, retry, 'succeeded' as status, instance_id FROM a RETURNING id";
    let res = sqlx::query(SQL).bind(job_id).execute(pool).await?;
    Ok(res.rows_affected())
}

pub async fn fail(pool: &Pool<Postgres>, job_id: i64) -> Result<u64, Error> {
    const SQL: &str = "WITH a AS (
            DELETE FROM enqueued WHERE id = $1 RETURNING id, retry, instance_id
        )
        INSERT INTO history SELECT id, retry, 'failed' as status, instance_id FROM a RETURNING id";
    let res = sqlx::query(SQL).bind(job_id).execute(pool).await?;
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

pub fn fetch_enqueued<'a>(
    pool: &'a Pool<Postgres>,
    instance_id: &'a str,
    prefetch: i32,
) -> BoxStream<'a, Result<JobQueueRow, sqlx::Error>> {
    const SQL: &str = "WITH a AS (
        SELECT id FROM enqueued WHERE lock_at IS NULL ORDER BY retry, id LIMIT $1 FOR UPDATE SKIP LOCKED
    )
    UPDATE enqueued SET instance_id = $2, lock_at = now() WHERE id = ANY(SELECT id FROM a) RETURNING id, retry";
    let res = sqlx::query_as::<_, JobQueueRow>(SQL)
        .bind(prefetch)
        .bind(instance_id)
        .fetch(pool);
    res
}

#[allow(dead_code, unused_variables)]
pub async fn fetch_optional(
    pool: &Pool<Postgres>,
    instance_id: &str,
) -> Result<Option<JobQueueRow>, sqlx::Error> {
    const SQL: &str = "WITH a AS (
        SELECT id FROM enqueued WHERE lock_at IS NULL ORDER BY retry, id LIMIT 1 FOR UPDATE SKIP LOCKED
    )
    UPDATE enqueued SET instance_id = $2, lock_at = now() WHERE id = ANY(SELECT id FROM a) RETURNING id, retry";
    let res = sqlx::query_as::<_, JobQueueRow>(SQL)
        .bind(instance_id)
        .fetch_optional(pool)
        .await;
    res
}
