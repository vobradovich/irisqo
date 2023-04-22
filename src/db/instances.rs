use crate::models::Error;
use sqlx::{Pool, Postgres};

pub async fn live(pool: &Pool<Postgres>, instance_id: &str) -> Result<(), Error> {
    sqlx::query("INSERT INTO instances(id) VALUES ($1) ON CONFLICT (id) DO UPDATE SET last_at = now()")
        .bind(instance_id)
        .execute(pool)
        .await?;
    Ok(())
}