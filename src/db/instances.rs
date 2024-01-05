use std::time::Duration;

use crate::models::Error;
use sqlx::{Pool, Postgres};

pub async fn live(pool: &Pool<Postgres>, instance_id: &str) -> Result<(), Error> {
    const SQL: &str =
        "INSERT INTO instances(id) VALUES ($1) ON CONFLICT (id) DO UPDATE SET last_at = now()";
    sqlx::query(SQL).bind(instance_id).execute(pool).await?;
    Ok(())
}

pub async fn kill_expired<'a>(pool: &'a Pool<Postgres>, expire: Duration) -> Result<u64, Error> {
    const SQL: &str = "
    WITH a AS (
        SELECT id FROM instances WHERE status = 'live' AND last_at < now() - $1 ORDER BY id FOR UPDATE SKIP LOCKED
    ), b AS (
        UPDATE enqueued SET instance_id = null, lock_at = null, retry = retry + 1 WHERE instance_id = ANY(SELECT id FROM a) RETURNING id    
    )
    UPDATE instances SET status = 'dead' WHERE id = ANY(SELECT id FROM a) RETURNING id";
    let res = sqlx::query(SQL).bind(expire).execute(pool).await?;
    Ok(res.rows_affected())
}

pub async fn kill<'a>(pool: &'a Pool<Postgres>, instance_id: &str) -> Result<u64, Error> {
    const SQL: &str = "UPDATE instances SET status = 'dead' WHERE id = $1 RETURNING id";
    let res = sqlx::query(SQL).bind(instance_id).execute(pool).await?;
    Ok(res.rows_affected())
}
