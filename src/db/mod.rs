use crate::models::Error;
use sqlx::{Pool, Postgres};
pub mod jobqueue;
pub mod instances;
pub mod results;

pub async fn select_one(pool: &Pool<Postgres>) -> Result<(), Error> {
    _ = sqlx::query("SELECT 1").fetch_one(pool).await?;
    Ok(())
}