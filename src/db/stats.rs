use crate::models::Error;
use sqlx::{Pool, Postgres};

/// The whole queue (all instances).
#[derive(Debug, Clone, Copy, sqlx::FromRow)]
pub struct QueueStats {
    pub scheduled: i64,
    pub enqueued: i64,
    pub processing: i64,
    /// Seconds the oldest due job has been waiting to start (0 when none).
    pub lag: f64,
}

pub async fn queue_stats(pool: &Pool<Postgres>) -> Result<QueueStats, Error> {
    const SQL: &str = "
    WITH e AS (
        -- one pass over enqueued
        SELECT
            count(*) FILTER (WHERE lock_at IS NULL) AS enqueued,
            count(*) FILTER (WHERE lock_at IS NOT NULL) AS processing,
            min(due_at) FILTER (WHERE lock_at IS NULL) AS oldest_due
        FROM enqueued
    )
    SELECT
        (SELECT count(*) FROM scheduled) AS scheduled,
        e.enqueued,
        e.processing,
        greatest(0, coalesce(greatest(
            extract(epoch FROM now() - e.oldest_due),
            (SELECT extract(epoch FROM now()) - min(at) FROM scheduled WHERE at <= extract(epoch FROM now())::bigint)
        ), 0))::float8 AS lag
    FROM e";
    let stats = sqlx::query_as::<_, QueueStats>(SQL).fetch_one(pool).await?;
    Ok(stats)
}
