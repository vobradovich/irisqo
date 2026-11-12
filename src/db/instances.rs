use std::time::Duration;

use crate::models::Error;
use sqlx::{Pool, Postgres};

/// Marks instances without a heartbeat for `expire` as dead and releases their jobs
/// (`attempt + 1`, so a late result from the dead instance is rejected).
pub async fn kill_expired(
    pool: &Pool<Postgres>,
    expire: Duration,
    instance_id: &str,
) -> Result<u64, Error> {
    const SQL: &str = "
    WITH a AS (
        SELECT instance_id FROM instances WHERE stopped_at IS NULL AND seen_at < now() - $1 ORDER BY instance_id FOR UPDATE SKIP LOCKED
    ), c AS (
        DELETE FROM claimed USING a WHERE claimed.instance_id = a.instance_id
        RETURNING claimed.id, claimed.attempt, claimed.flow_key, claimed.priority, a.instance_id AS locked_by
    ), b AS (
        INSERT INTO enqueued(id, attempt, flow_key, priority) SELECT id, attempt + 1, flow_key, priority FROM c
    ), hist AS (
        INSERT INTO history(id, attempt, kind, instance_id, message)
        SELECT id, attempt + 1, 'retried', $2, 'instance expired: ' || locked_by FROM c
    )
    UPDATE instances SET stopped_at = now() WHERE instance_id IN (SELECT instance_id FROM a) RETURNING instance_id";
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
        DELETE FROM claimed WHERE instance_id = $1 RETURNING id, attempt, flow_key, priority
    ), b AS (
        INSERT INTO enqueued(id, attempt, flow_key, priority) SELECT id, attempt, flow_key, priority FROM a
    ), hist AS (
        INSERT INTO history(id, attempt, kind, instance_id, message)
        SELECT id, attempt, 'released', $1, 'released on shutdown' FROM a
    )
    UPDATE instances SET stopped_at = now() WHERE instance_id = $1 RETURNING instance_id
    ";
    let res = sqlx::query(SQL).bind(instance_id).execute(pool).await?;
    Ok(res.rows_affected())
}
