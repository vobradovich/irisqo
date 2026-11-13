use crate::models::Error;
use sqlx::{Pool, Postgres};

/// The whole queue (all instances).
#[derive(Debug, Clone, Copy, sqlx::FromRow)]
pub struct QueueStats {
    pub scheduled: i64,
    /// Everything in `enqueued`.
    pub enqueued: i64,
    /// Of the enqueued, the jobs of a flow: they wait for the flow's limit (concurrency, rate,
    /// pause) or for a worker. The rest wait for a worker.
    pub held: i64,
    pub processing: i64,
    /// Seconds the oldest due job has been waiting to start (0 when none). Jobs of a flow do
    /// not count: a flow holds them by design, and only a lack of workers is a lag.
    pub lag: f64,
}

pub async fn queue_stats(pool: &Pool<Postgres>) -> Result<QueueStats, Error> {
    const SQL: &str = "
    SELECT
        (SELECT count(*) FROM scheduled) AS scheduled,
        e.enqueued,
        e.held,
        (SELECT count(*) FROM claimed) AS processing,
        greatest(0, coalesce(extract(epoch FROM now() - (
            SELECT min(d.at) FROM (
                SELECT e.due AS at
                UNION ALL
                SELECT min(at) FROM scheduled WHERE at <= now()
            ) AS d
        )), 0))::float8 AS lag
    FROM (SELECT count(*) AS enqueued,
                 count(*) FILTER (WHERE flow_key IS NOT NULL) AS held,
                 min(at) FILTER (WHERE flow_key IS NULL) AS due
          FROM enqueued) AS e";
    let stats = sqlx::query_as::<_, QueueStats>(SQL).fetch_one(pool).await?;
    Ok(stats)
}
