use crate::{
    db,
    models::{Error, JobMeta},
};

use sqlx::{Pool, Postgres};

use super::{JobSchedule, ScheduleRow};

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

/// Pauses a schedule: no more runs, and its pending run (if it is not running) is
/// cancelled. Returns `false` when there is no such schedule.
pub async fn pause(
    pool: &Pool<Postgres>,
    schedule_id: &str,
    instance_id: &str,
) -> Result<bool, Error> {
    const SQL: &str =
        "UPDATE schedules SET inactive = true WHERE schedule_id = $1 RETURNING next_id";
    let mut tx = pool.begin().await?;
    let Some(next_id) = sqlx::query_scalar::<_, Option<i64>>(SQL)
        .bind(schedule_id)
        .fetch_optional(&mut *tx)
        .await?
    else {
        return Ok(false);
    };
    if let Some(next_id) = next_id {
        db::jobqueue::cancel(&mut *tx, next_id, instance_id).await?;
    }
    tx.commit().await?;
    Ok(true)
}

/// Resumes a schedule: when it has no pending run, the next one is scheduled
/// (unless its `until` has passed). Returns `false` when there is no such schedule.
pub async fn resume(
    pool: &Pool<Postgres>,
    schedule_id: &str,
    now_secs: i64,
    instance_id: &str,
) -> Result<bool, Error> {
    const SQL: &str = "
    UPDATE schedules AS s SET inactive = false
    WHERE schedule_id = $1
    RETURNING s.schedule, s.until, s.next_id,
        EXISTS (SELECT 1 FROM scheduled WHERE id = s.next_id)
            OR EXISTS (SELECT 1 FROM enqueued WHERE id = s.next_id) AS pending";
    #[derive(sqlx::FromRow)]
    struct Resumed {
        schedule: String,
        until: Option<i64>,
        next_id: Option<i64>,
        pending: bool,
    }
    let mut tx = pool.begin().await?;
    let Some(Resumed {
        schedule,
        until,
        next_id,
        pending,
    }) = sqlx::query_as::<_, Resumed>(SQL)
        .bind(schedule_id)
        .fetch_optional(&mut *tx)
        .await?
    else {
        return Ok(false);
    };
    if !pending
        && let Some(next_id) = next_id
        && let Some(at) = schedule
            .parse::<JobSchedule>()
            .map_err(|e| Error::InvalidParams(vec![e]))?
            .next(now_secs, until)
    {
        db::jobqueue::clone_schedule_at(&mut *tx, next_id, at, instance_id).await?;
    }
    tx.commit().await?;
    Ok(true)
}

/// A schedule with its request and its last processed run.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ScheduleView {
    pub schedule_id: String,
    pub schedule: String,
    pub until: Option<i64>,
    pub next_id: Option<i64>,
    pub next_at: Option<i64>,
    pub inactive: bool,
    #[sqlx(json(nullable))]
    pub meta: Option<JobMeta>,
    /// Its next run is waiting (scheduled or enqueued).
    pub pending: bool,
    pub last_id: Option<i64>,
    pub last_status: Option<String>,
    pub last_at: Option<chrono::DateTime<chrono::Utc>>,
}

pub async fn list_view(pool: &Pool<Postgres>, limit: i32) -> Result<Vec<ScheduleView>, Error> {
    const SQL: &str = "
    SELECT s.schedule_id, s.schedule, s.until, s.next_id, s.next_at, s.inactive, j.meta,
        (sc.id IS NOT NULL OR e.id IS NOT NULL) AS pending,
        lp.id AS last_id, lp.status AS last_status, lp.at AS last_at
    FROM schedules AS s
    LEFT JOIN jobs AS j ON j.id = s.next_id
    LEFT JOIN scheduled AS sc ON sc.id = s.next_id
    LEFT JOIN enqueued AS e ON e.id = s.next_id
    LEFT JOIN LATERAL (
        SELECT p.id, p.status::text AS status, p.at
        FROM jobs AS j2 INNER JOIN processed AS p ON p.id = j2.id
        WHERE j2.schedule_id = s.schedule_id
        ORDER BY p.at DESC LIMIT 1
    ) AS lp ON true
    ORDER BY s.schedule_id DESC
    LIMIT $1";
    let rows = sqlx::query_as::<_, ScheduleView>(SQL)
        .bind(limit)
        .fetch_all(pool)
        .await?;
    Ok(rows)
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
