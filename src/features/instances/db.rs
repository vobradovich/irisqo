use crate::models::Error;
use serde::Serialize;
use sqlx::{Pool, Postgres};

async fn live(pool: &Pool<Postgres>, instance_id: &str) -> Result<(), Error> {
    const SQL: &str = "INSERT INTO instances(id) VALUES ($1) ON CONFLICT (id) DO UPDATE SET last_at = now()";
    sqlx::query(SQL).bind(instance_id).execute(pool).await?;
    Ok(())
}

pub(crate) async fn get_all<'a>(
    pool: &'a Pool<Postgres>,
    limit: i32,
    offset: i32,
) -> Result<Vec<InstanceRow>, Error> {
    const SQL: &str = "SELECT id, status::text, last_at FROM instances ORDER BY id DESC LIMIT $1 OFFSET $2";
    let res = sqlx::query_as::<_, InstanceRow>(SQL)
        .bind(limit)
        .bind(offset)
        .fetch_all(pool)
        .await?;
    Ok(res)
}

#[derive(Debug, Clone, sqlx::FromRow, Serialize)]
pub struct InstanceRow {
    pub id: String,
    pub status: String,
    pub last_at: chrono::DateTime<chrono::Utc>,
}
