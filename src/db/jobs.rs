use crate::models::Error;
use crate::models::{JobRow, JobQueueEntry};
use futures::stream::BoxStream;
use sqlx::{Pool, Postgres};

pub async fn enqueue(pool: &Pool<Postgres>, protocol: &str, url: &str) -> Result<i64, Error> {
    const SQL: &str = "WITH a AS (
        INSERT INTO jobs(protocol, url) VALUES ($1, $2) RETURNING id
    )
    INSERT INTO enqueued SELECT id FROM a RETURNING id";
    let job_id = sqlx::query_scalar::<_, i64>(SQL)
        .bind(protocol)
        .bind(url)
        .fetch_one(pool)
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

pub async fn delete(pool: &Pool<Postgres>, job_id: i64) -> Result<u64, Error> {
    const SQL: &str = "DELETE FROM jobs WHERE id = $1";
    let res = sqlx::query(SQL)
        .bind(job_id)
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}

pub async fn enqueue_scheduled(pool: &Pool<Postgres>) -> Result<u64, Error> {
    const SQL: &str = "WITH b AS (
        WITH a AS (
            SELECT id, retry FROM scheduled WHERE at <= now() ORDER BY retry, id LIMIT $1 FOR UPDATE SKIP LOCKED
        )
        INSERT INTO enqueued SELECT id, retry FROM a RETURNING id
    )
    DELETE FROM scheduled WHERE id = ANY(SELECT id FROM b)";
    let res = sqlx::query(SQL)
        .bind(1000)
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}

pub async fn succeed(pool: &Pool<Postgres>, job_id: i64) -> Result<u64, Error> {
    const SQL: &str = "WITH a AS (
        DELETE FROM enqueued WHERE id = $1 RETURNING id, retry, instance_id
    )
    INSERT INTO history SELECT id, retry, 'succeeded' as status, instance_id FROM a RETURNING id";
    let res = sqlx::query(SQL)
        .bind(job_id)
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}

#[allow(dead_code, unused_variables)]
pub async fn fail(pool: &Pool<Postgres>, job_id: i64) -> Result<u64, Error> {
    const SQL: &str = "WITH b AS (
        WITH a AS (
            SELECT id, retry FROM enqueued WHERE id = $1 FOR UPDATE SKIP LOCKED
        )
        INSERT INTO failed SELECT id, retry FROM a RETURNING id
    )
    DELETE FROM enqueued WHERE id = $1";
    let res = sqlx::query(SQL)
        .bind(1000)
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}

#[allow(dead_code, unused_variables)]
pub async fn assign_enqueued(pool: &Pool<Postgres>, instance_id: &str, prefetch: i32) -> Result<i64, Error> {
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

pub fn fetch_enqueued<'a>(pool: &'a Pool<Postgres>, instance_id: &'a str, prefetch: i32) -> BoxStream<'a, Result<JobQueueEntry, sqlx::Error>> {
    const SQL: &str = "WITH a AS (
        SELECT id FROM enqueued WHERE lock_at IS NULL ORDER BY retry, id LIMIT $1 FOR UPDATE SKIP LOCKED
    )
    UPDATE enqueued SET instance_id = $2, lock_at = now() WHERE id = ANY(SELECT id FROM a) RETURNING id, retry";
    let res = sqlx::query_as::<_, JobQueueEntry>(SQL)
        .bind(prefetch)
        .bind(instance_id)
        .fetch(pool);
    res
}

#[allow(dead_code, unused_variables)]
pub async fn fetch_optional(pool: &Pool<Postgres>, instance_id: &str) -> Result<Option<JobQueueEntry>, sqlx::Error> {
    const SQL: &str = "WITH a AS (
        SELECT id FROM enqueued WHERE lock_at IS NULL ORDER BY retry, id LIMIT 1 FOR UPDATE SKIP LOCKED
    )
    UPDATE enqueued SET instance_id = $2, lock_at = now() WHERE id = ANY(SELECT id FROM a) RETURNING id, retry";
    let res = sqlx::query_as::<_, JobQueueEntry>(SQL)
        .bind(instance_id)
        .fetch_optional(pool)
        .await;
    res
}
