use crate::models::Error;
use crate::models::JobResultRow;

use sqlx::{Pool, Postgres};

pub async fn get_by_id(pool: &Pool<Postgres>, job_id: i64) -> Result<Option<JobResultRow>, Error> {
    const SQL: &str = "SELECT * FROM processed WHERE id = $1";
    let job = sqlx::query_as::<_, JobResultRow>(SQL)
        .bind(job_id)
        .fetch_optional(pool)
        .await?;
    Ok(job)
}
