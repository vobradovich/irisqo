use sqlx::{Pool, Postgres};

use super::{QueueCreate, QueueRow, UndeclaredQueueRow};
use crate::models::Error;

/// A page of the declared queues by name: straight from `queues`. `after` is the last name of
/// the previous page.
pub async fn list(
    pool: &Pool<Postgres>,
    after: Option<&str>,
    limit: i64,
    offset: i64,
) -> Result<Vec<QueueRow>, Error> {
    const SQL: &str = "
    SELECT queue_id, parallelism, ordered, rate,
           extract(epoch FROM rate_period)::bigint AS rate_period, paused, next_at
    FROM queues
    WHERE $1::text IS NULL OR queue_id > $1
    ORDER BY queue_id
    LIMIT $2 OFFSET $3";
    let rows = sqlx::query_as::<_, QueueRow>(SQL)
        .bind(after)
        .bind(limit)
        .bind(offset)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

/// A page of the names that jobs use without a declaration, by name, with their jobs counted.
pub async fn list_undeclared(
    pool: &Pool<Postgres>,
    after: Option<&str>,
    limit: i64,
    offset: i64,
) -> Result<Vec<UndeclaredQueueRow>, Error> {
    const SQL: &str = "
    WITH e AS (
        SELECT queue, count(*) FILTER (WHERE at <= now()) AS ready,
               count(*) FILTER (WHERE at > now()) AS delayed
        FROM enqueued
        WHERE queue IS NOT NULL AND ($1::text IS NULL OR queue > $1)
        GROUP BY queue
    ), c AS (
        SELECT queue, count(*) AS processing
        FROM claimed
        WHERE queue IS NOT NULL AND ($1::text IS NULL OR queue > $1)
        GROUP BY queue
    ), n AS (
        SELECT queue FROM e UNION SELECT queue FROM c
    )
    SELECT n.queue AS queue_id,
        coalesce(e.ready, 0)::bigint AS ready,
        coalesce(e.delayed, 0)::bigint AS delayed,
        coalesce(c.processing, 0)::bigint AS processing
    FROM n
    LEFT JOIN e ON e.queue = n.queue
    LEFT JOIN c ON c.queue = n.queue
    WHERE NOT EXISTS (SELECT 1 FROM queues q WHERE q.queue_id = n.queue)
    ORDER BY n.queue
    LIMIT $2 OFFSET $3";
    let rows = sqlx::query_as::<_, UndeclaredQueueRow>(SQL)
        .bind(after)
        .bind(limit)
        .bind(offset)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

/// A declared queue.
pub async fn get_by_id(pool: &Pool<Postgres>, queue: &str) -> Result<Option<QueueRow>, Error> {
    const SQL: &str = "
    SELECT queue_id, parallelism, ordered, rate,
           extract(epoch FROM rate_period)::bigint AS rate_period, paused, next_at
    FROM queues WHERE queue_id = $1";
    let row = sqlx::query_as::<_, QueueRow>(SQL)
        .bind(queue)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

/// Declares a queue or replaces the limits of a declared one (the pause stays; the rate limit
/// starts afresh, a new rate takes effect at once). Gives the queue and whether it is new.
pub async fn upsert(pool: &Pool<Postgres>, queue: &QueueCreate) -> Result<(QueueRow, bool), Error> {
    #[derive(sqlx::FromRow)]
    struct Upserted {
        #[sqlx(flatten)]
        row: QueueRow,
        created: bool,
    }
    const SQL: &str = "
    INSERT INTO queues(queue_id, parallelism, ordered, rate, rate_period)
    VALUES ($1, $2, $3, $4, make_interval(secs => $5::float8))
    ON CONFLICT (queue_id) DO UPDATE
        SET parallelism = EXCLUDED.parallelism, ordered = EXCLUDED.ordered,
            rate = EXCLUDED.rate, rate_period = EXCLUDED.rate_period, next_at = NULL
    RETURNING queue_id, parallelism, ordered, rate,
              extract(epoch FROM rate_period)::bigint AS rate_period, paused, next_at,
              (xmax = 0) AS created";
    let done = sqlx::query_as::<_, Upserted>(SQL)
        .bind(&queue.queue)
        .bind(queue.parallelism)
        .bind(queue.ordered)
        .bind(queue.rate)
        .bind(f64::from(queue.rate_period))
        .fetch_one(pool)
        .await?;
    Ok((done.row, done.created))
}

/// Pauses a declared queue: its jobs wait, none is lost. `false` when it is not declared.
pub async fn pause(pool: &Pool<Postgres>, queue: &str) -> Result<bool, Error> {
    set_paused(pool, queue, true).await
}

/// Resumes a declared queue. `false` when it is not declared.
pub async fn resume(pool: &Pool<Postgres>, queue: &str) -> Result<bool, Error> {
    set_paused(pool, queue, false).await
}

async fn set_paused(pool: &Pool<Postgres>, queue: &str, paused: bool) -> Result<bool, Error> {
    let done = sqlx::query("UPDATE queues SET paused = $2 WHERE queue_id = $1")
        .bind(queue)
        .bind(paused)
        .execute(pool)
        .await?;
    Ok(done.rows_affected() > 0)
}

/// Removes the declaration: the jobs stay and the name goes back to one job at a time, in order.
pub async fn delete(pool: &Pool<Postgres>, queue: &str) -> Result<bool, Error> {
    let done = sqlx::query("DELETE FROM queues WHERE queue_id = $1")
        .bind(queue)
        .execute(pool)
        .await?;
    Ok(done.rows_affected() > 0)
}
