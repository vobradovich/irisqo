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
        (SELECT count(*) FROM enqueued) AS enqueued,
        (SELECT count(*) FROM claimed) AS processing,
        greatest(0, coalesce(extract(epoch FROM now() - (
            SELECT min(d.at) FROM (
                SELECT min(at) AS at FROM enqueued WHERE at <= now()
                UNION ALL
                SELECT min(at) FROM scheduled WHERE at <= now()
            ) AS d
        )), 0))::float8 AS lag";
    let stats = sqlx::query_as::<_, QueueStats>(SQL).fetch_one(pool).await?;
    Ok(stats)
}
