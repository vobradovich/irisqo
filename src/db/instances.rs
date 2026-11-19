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
        UPDATE queue AS q SET state = 'ready', instance_id = NULL, attempt = q.attempt + 1, at = now()
        FROM a WHERE q.state = 'claimed' AND q.instance_id = a.instance_id
        RETURNING q.id, q.attempt, a.instance_id AS locked_by
    ), hist AS (
        INSERT INTO history(id, attempt, kind, instance_id, message)
        SELECT id, attempt, 'retried', $2, 'instance expired: ' || locked_by FROM c
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
        UPDATE queue SET state = 'ready', instance_id = NULL, at = now()
        WHERE state = 'claimed' AND instance_id = $1
        RETURNING id, attempt
    ), hist AS (
        INSERT INTO history(id, attempt, kind, instance_id, message)
        SELECT id, attempt, 'released', $1, 'released on shutdown' FROM a
    )
    UPDATE instances SET stopped_at = now() WHERE instance_id = $1 RETURNING instance_id
    ";
    let res = sqlx::query(SQL).bind(instance_id).execute(pool).await?;
    Ok(res.rows_affected())
}
