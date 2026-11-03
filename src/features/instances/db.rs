use crate::models::Error;
use serde::Serialize;
use sqlx::{Pool, Postgres};

/// Upsert an instance heartbeat, updating `last_at` when it already exists.
pub async fn live(pool: &Pool<Postgres>, instance_id: &str) -> Result<(), Error> {
    const SQL: &str = "
    INSERT INTO instances(instance_id) VALUES ($1) ON CONFLICT (instance_id) DO UPDATE SET last_at = now(), status = 'live'
    ";
    sqlx::query(SQL).bind(instance_id).execute(pool).await?;
    Ok(())
}

/// Fetch a page of instances ordered by newest id first.
pub(crate) async fn get_all(
    pool: &Pool<Postgres>,
    limit: i32,
    offset: i32,
) -> Result<Vec<InstanceRow>, Error> {
    const SQL: &str = "
    SELECT instance_id, status::text, last_at FROM instances ORDER BY instance_id DESC LIMIT $1 OFFSET $2
    ";
    let res = sqlx::query_as::<_, InstanceRow>(SQL)
        .bind(limit)
        .bind(offset)
        .fetch_all(pool)
        .await?;
    Ok(res)
}

/// Row entity for `instances` queries.
#[derive(Debug, Clone, sqlx::FromRow, Serialize)]
pub struct InstanceRow {
    pub instance_id: String,
    pub status: String,
    pub last_at: chrono::DateTime<chrono::Utc>,
}
