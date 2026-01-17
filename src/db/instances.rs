use std::time::Duration;

use crate::models::Error;
use sqlx::{Pool, Postgres};

pub async fn kill_expired(pool: &Pool<Postgres>, expire: Duration) -> Result<u64, Error> {
    const SQL: &str = "
    WITH a AS (
        SELECT instance_id FROM instances WHERE status = 'live' AND last_at < now() - $1 ORDER BY instance_id FOR UPDATE SKIP LOCKED
    ), b AS (
        UPDATE enqueued SET instance_id = null, lock_at = null, retry = retry + 1 WHERE instance_id = ANY(SELECT instance_id FROM a) RETURNING instance_id
    )
    UPDATE instances SET status = 'dead' WHERE instance_id = ANY(SELECT instance_id FROM a) RETURNING instance_id";
    let res = sqlx::query(SQL).bind(expire).execute(pool).await?;
    Ok(res.rows_affected())
}

pub async fn kill(pool: &Pool<Postgres>, instance_id: &str) -> Result<u64, Error> {
    const SQL: &str = "
    UPDATE instances SET status = 'dead' WHERE instance_id = $1 RETURNING instance_id
    ";
    let res = sqlx::query(SQL).bind(instance_id).execute(pool).await?;
    Ok(res.rows_affected())
}
