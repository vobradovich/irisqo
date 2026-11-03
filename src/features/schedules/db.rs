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

pub async fn get_all(
    pool: &Pool<Postgres>,
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

pub async fn inactive(
    pool: &Pool<Postgres>,
    schedule_id: &str,
    inactive: bool,
) -> Result<u64, Error> {
    const SQL: &str = "
    UPDATE schedules SET inactive = $2 WHERE schedule_id = $1 RETURNING schedule_id
    ";
    let res = sqlx::query(SQL)
        .bind(schedule_id)
        .bind(inactive)
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}

/// Deletes the schedule with its pending (not yet enqueued) next run.
/// Already processed runs stay, with `schedule_id` set to NULL.
pub async fn delete(pool: &Pool<Postgres>, schedule_id: &str) -> Result<u64, Error> {
    const SQL_NEXT: &str = "
    DELETE FROM jobs WHERE id IN (
        SELECT sc.id FROM scheduled AS sc
        INNER JOIN schedules AS s ON s.next_id = sc.id
        WHERE s.schedule_id = $1
    )";
    const SQL: &str = "DELETE FROM schedules WHERE schedule_id = $1";
    let mut tx = pool.begin().await?;
    sqlx::query(SQL_NEXT)
        .bind(schedule_id)
        .execute(&mut *tx)
        .await?;
    let res = sqlx::query(SQL).bind(schedule_id).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(res.rows_affected())
}
