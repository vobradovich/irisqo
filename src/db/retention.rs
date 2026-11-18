//! Deletes old data in batches. A Postgres partitioning of `jobs`/`processed`/`history`
//! by `id` range could replace it with dropping partitions (see `sql/queries.sql`).

use std::time::Duration;

use crate::models::Error;
use sqlx::{Pool, Postgres};

/// Deletes up to `limit` jobs processed more than `older_than` ago;
/// their `processed` and `history` rows go with them (ON DELETE CASCADE).
/// A job retried or deleted by a user (its `processed` or `jobs` row locked) is skipped, never waited for.
pub async fn purge_processed(
    pool: &Pool<Postgres>,
    older_than: Duration,
    limit: i32,
) -> Result<u64, Error> {
    const SQL: &str = "
    DELETE FROM jobs WHERE id IN (
        SELECT j.id FROM processed AS p INNER JOIN jobs AS j ON j.id = p.id
        WHERE p.at < now() - $1 ORDER BY p.at LIMIT $2
        FOR UPDATE OF j, p SKIP LOCKED
    )";
    let res = sqlx::query(SQL)
        .bind(older_than)
        .bind(limit)
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}

/// Deletes schedules with no jobs left: finished (or stopped) ones whose runs were purged.
/// An active schedule always has its pending next run.
/// A schedule whose `next_id` a concurrent `upsert` changed (`s` fresh vs `c` snapshot) is kept.
pub async fn purge_schedules(pool: &Pool<Postgres>) -> Result<u64, Error> {
    const SQL: &str = "
    DELETE FROM schedules AS s
    USING (
        SELECT o.schedule_id, o.next_id FROM schedules AS o
        WHERE NOT EXISTS (SELECT 1 FROM jobs AS j WHERE j.schedule_id = o.schedule_id)
    ) AS c
    WHERE s.schedule_id = c.schedule_id AND s.next_id IS NOT DISTINCT FROM c.next_id";
    let res = sqlx::query(SQL).execute(pool).await?;
    Ok(res.rows_affected())
}

/// Deletes instances dead for more than `older_than`.
pub async fn purge_instances(pool: &Pool<Postgres>, older_than: Duration) -> Result<u64, Error> {
    const SQL: &str = "DELETE FROM instances WHERE stopped_at < now() - $1";
    let res = sqlx::query(SQL).bind(older_than).execute(pool).await?;
    Ok(res.rows_affected())
}
