use crate::models::Error;

use sqlx::{Pool, Postgres};

use super::ScheduleRow;

pub async fn get_by_id(
    pool: &Pool<Postgres>,
    schedule_id: &str,
) -> Result<Option<ScheduleRow>, Error> {
    const SQL: &str = "SELECT * FROM schedules WHERE schedule_id = $1";
    let row = sqlx::query_as::<_, ScheduleRow>(SQL)
        .bind(schedule_id)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

pub async fn get_all<'a>(
    pool: &'a Pool<Postgres>,
    limit: i32,
    offset: i32,
) -> Result<Vec<ScheduleRow>, Error> {
    const SQL: &str = "SELECT * FROM schedules ORDER BY schedule_id LIMIT $1 OFFSET $2;";
    let res = sqlx::query_as::<_, ScheduleRow>(SQL)
        .bind(limit)
        .bind(offset)
        .fetch_all(pool)
        .await?;
    Ok(res)
}

pub async fn disable(
    pool: &Pool<Postgres>,
    schedule_id: &str,
) -> Result<u64, Error> {
    const SQL: &str = "UPDATE schedules SET inactive = TRUE WHERE id = $1 RETURNING id";
    let res = sqlx::query(SQL)
        .bind(schedule_id)
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}
