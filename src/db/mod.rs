use crate::models::Error;
use sqlx::{Pool, Postgres};
pub mod jobs;
pub mod instances;

pub async fn select_one(pool: &Pool<Postgres>) -> Result<(), Error> {
    _ = sqlx::query("SELECT 1").fetch_one(pool).await?;
    Ok(())
}