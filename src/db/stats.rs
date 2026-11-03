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
    SELECT
        (SELECT count(*) FROM scheduled) AS scheduled,
        (SELECT count(*) FROM enqueued WHERE lock_at IS NULL) AS enqueued,
        (SELECT count(*) FROM enqueued WHERE lock_at IS NOT NULL) AS processing,
        greatest(0, coalesce(greatest(
            (SELECT extract(epoch FROM now() - min(due_at)) FROM enqueued WHERE lock_at IS NULL),
            (SELECT extract(epoch FROM now()) - min(at) FROM scheduled WHERE at <= extract(epoch FROM now())::bigint)
        ), 0))::float8 AS lag";
    let stats = sqlx::query_as::<_, QueueStats>(SQL).fetch_one(pool).await?;
    Ok(stats)
}
