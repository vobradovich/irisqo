use std::time::Duration;

use crate::models::Error;
use sqlx::{Pool, Postgres};

/// Marks instances without a heartbeat for `expire` as dead and releases their jobs
/// (`retry + 1`, so a late result from the dead instance is rejected).
pub async fn kill_expired(
    pool: &Pool<Postgres>,
    expire: Duration,
    instance_id: &str,
) -> Result<u64, Error> {
    const SQL: &str = "
    WITH a AS (
        SELECT instance_id FROM instances WHERE status = 'live' AND last_at < now() - $1 ORDER BY instance_id FOR UPDATE SKIP LOCKED
    ), b AS (
        UPDATE enqueued AS e SET instance_id = null, lock_at = null, retry = e.retry + 1
        FROM a WHERE e.instance_id = a.instance_id
        RETURNING e.id, e.retry, a.instance_id AS locked_by
    ), hist AS (
        INSERT INTO history(id, retry, instance_id, status, message)
        SELECT id, retry, $2, 'retried'::history_status, 'instance expired: ' || locked_by FROM b
    )
    UPDATE instances SET status = 'dead' WHERE instance_id = ANY(SELECT instance_id FROM a) RETURNING instance_id";
    let res = sqlx::query(SQL)
        .bind(expire)
        .bind(instance_id)
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}

/// Graceful shutdown: releases the jobs still locked by this instance and marks it dead.
pub async fn kill(pool: &Pool<Postgres>, instance_id: &str) -> Result<u64, Error> {
    const SQL: &str = "
    WITH a AS (
        UPDATE enqueued SET instance_id = null, lock_at = null WHERE instance_id = $1 RETURNING id, retry
    ), hist AS (
        INSERT INTO history(id, retry, instance_id, status, message)
        SELECT id, retry, $1, 'enqueued'::history_status, 'released on shutdown' FROM a
    )
    UPDATE instances SET status = 'dead' WHERE instance_id = $1 RETURNING instance_id
    ";
    let res = sqlx::query(SQL).bind(instance_id).execute(pool).await?;
    Ok(res.rows_affected())
}
