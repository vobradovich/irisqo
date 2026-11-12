use crate::{
    db,
    models::{Error, JobCreate, JobCreateRow, JobMeta},
};

use chrono::{DateTime, Utc};
use sqlx::{Pool, Postgres, types::Json};

use super::{JobSchedule, ScheduleRow};

pub async fn get_by_id(
    pool: &Pool<Postgres>,
    schedule_id: &str,
) -> Result<Option<ScheduleRow>, Error> {
    const SQL: &str = "SELECT schedule_id, rule, tz, until, next_id, next_at, paused FROM schedules WHERE schedule_id = $1";
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
    const SQL: &str = "SELECT schedule_id, rule, tz, until, next_id, next_at, paused FROM schedules ORDER BY schedule_id LIMIT $1 OFFSET $2";
    let res = sqlx::query_as::<_, ScheduleRow>(SQL)
        .bind(limit)
        .bind(offset)
        .fetch_all(pool)
        .await?;
    Ok(res)
}

/// Declares a schedule: `job.key` is its id, `job.schedule` its rule. A new schedule gets its
/// first run. An existing one keeps its id and takes the new rule and job: its waiting run is
/// cancelled, as by a pause, and replaced by a new one (a paused schedule is resumed). A run
/// that is already running goes on, but does not continue the schedule. A job is never changed:
/// the new run is a new job.
pub async fn upsert(
    pool: &Pool<Postgres>,
    job: JobCreate,
    instance_id: &str,
) -> Result<JobCreateRow, Error> {
    // the row is created, or its rule is changed and it stays locked until the commit: two
    // declarations of a schedule follow each other; it gives the run that is waiting
    const SQL_SCHEDULE: &str = "
    INSERT INTO schedules(schedule_id, rule, tz, until, paused)
    VALUES ($1, $2, $3, $4::timestamptz, false)
    ON CONFLICT (schedule_id) DO UPDATE
        SET rule = EXCLUDED.rule, tz = EXCLUDED.tz,
            until = EXCLUDED.until, paused = false
    RETURNING next_id";
    const SQL_RUN: &str = "
    WITH a AS (
        INSERT INTO jobs(meta, headers, body, key, schedule_id) VALUES ($1, $2, $3, $4, $5) RETURNING id
    ), b AS (
        UPDATE schedules SET next_id = a.id, next_at = $6::timestamptz
        FROM a WHERE schedules.schedule_id = $5
    ), hist AS (
        INSERT INTO history(id, attempt, kind, instance_id) SELECT id, 0, 'scheduled', $7 FROM a
    )
    INSERT INTO scheduled(id, at, flow_key, priority) SELECT id, $6::timestamptz, $8, $9 FROM a RETURNING id";

    let (Some(schedule_id), Some(schedule)) = (job.key.as_deref(), job.schedule.as_ref()) else {
        return Err(Error::invalid("schedule", "needs an id and a rule"));
    };
    let Some(at) = schedule.next(job.at.unwrap_or_else(Utc::now)) else {
        return Err(Error::invalid(
            "until",
            "the schedule has no run before it ends",
        ));
    };
    let (schedule_id, rule, tz, until) = (
        schedule_id.to_owned(),
        schedule.rule.to_string(),
        schedule.tz,
        schedule.until,
    );

    let mut tx = pool.begin().await?;
    let next_id = sqlx::query_scalar::<_, Option<i64>>(SQL_SCHEDULE)
        .bind(&schedule_id)
        .bind(&rule)
        .bind(tz.name())
        .bind(until)
        .fetch_one(&mut *tx)
        .await?;
    if let Some(next_id) = next_id {
        db::jobqueue::cancel(&mut *tx, next_id, instance_id).await?;
    }
    // the key of a run is `{schedule_id}-{run time, secs}`: a schedule has at most one run in a
    // second, and the run that was cancelled above gave its key back
    let key = format!("{schedule_id}-{}", at.timestamp());
    let (flow, priority) = (job.meta.flow_key.as_ref(), job.meta.priority.unwrap_or(0));
    let id = sqlx::query_scalar::<_, i64>(SQL_RUN)
        .bind(Json(&job.meta))
        .bind(Json(&job.headers))
        .bind(job.body_bytes())
        .bind(&key)
        .bind(&schedule_id)
        .bind(at)
        .bind(instance_id)
        .bind(flow)
        .bind(priority)
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(JobCreateRow {
        id,
        schedule_id: Some(schedule_id),
        key: Some(key),
    })
}

/// Clones the job as the next run of its schedule.
/// Returns `None` when the schedule was deleted or deactivated in the meantime,
/// or when the job is not its latest run.
///
/// The row of the schedule is locked first (`FOR UPDATE`): a declaration that is in flight is
/// waited for, and when it has replaced the next run, the job is not the next run any more and
/// nothing is created. The key of a run is `{schedule_id}-{run time, secs}`: a schedule has at
/// most one run in a second, and a run that was cancelled gave its key back (see [`db::jobqueue::cancel`]).
pub async fn clone_schedule_at<'e, E>(
    executor: E,
    job_id: i64,
    at: DateTime<Utc>,
    instance_id: &str,
) -> Result<Option<i64>, Error>
where
    E: sqlx::Executor<'e, Database = Postgres>,
{
    const SQL: &str = "
    WITH a AS (
        INSERT INTO jobs(meta, headers, body, schedule_id, key)
        SELECT j.meta, j.headers, j.body, j.schedule_id,
            j.schedule_id || '-' || extract(epoch FROM $2::timestamptz)::bigint
        FROM jobs AS j
        INNER JOIN schedules AS s ON s.schedule_id = j.schedule_id
        -- only the latest run continues the schedule (a manually retried old run does not)
        WHERE j.id = $1 AND s.next_id = j.id AND NOT s.paused
        FOR UPDATE OF s
        RETURNING id, schedule_id, meta->>'flow_key' AS flow_key,
            coalesce((meta->>'priority')::int, 0) AS priority
    ), b AS (
        UPDATE schedules
        SET next_id = a.id, next_at = $2::timestamptz
        FROM a
        WHERE schedules.schedule_id = a.schedule_id
    ), hist AS (
        INSERT INTO history(id, attempt, kind, instance_id) SELECT id, 0, 'scheduled', $3 FROM a
    )
    INSERT INTO scheduled(id, at, flow_key, priority)
    SELECT a.id, $2::timestamptz, a.flow_key, a.priority FROM a RETURNING id
    ";

    let job_id = sqlx::query_scalar::<_, i64>(SQL)
        .bind(job_id)
        .bind(at)
        .bind(instance_id)
        .fetch_optional(executor)
        .await?;
    Ok(job_id)
}

/// Pauses a schedule: no more runs, and its pending run (if it is not running) is
/// cancelled. Returns `false` when there is no such schedule.
pub async fn pause(
    pool: &Pool<Postgres>,
    schedule_id: &str,
    instance_id: &str,
) -> Result<bool, Error> {
    const SQL: &str = "UPDATE schedules SET paused = true WHERE schedule_id = $1 RETURNING next_id";
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
    now: DateTime<Utc>,
    instance_id: &str,
) -> Result<bool, Error> {
    const SQL: &str = "
    UPDATE schedules AS s SET paused = false
    WHERE schedule_id = $1
    RETURNING s.rule, s.tz, s.until, s.next_id, s.next_at,
        EXISTS (SELECT 1 FROM scheduled WHERE id = s.next_id)
            OR EXISTS (SELECT 1 FROM enqueued WHERE id = s.next_id)
            OR EXISTS (SELECT 1 FROM claimed WHERE id = s.next_id) AS pending";
    #[derive(sqlx::FromRow)]
    struct Resumed {
        rule: String,
        tz: String,
        until: Option<DateTime<Utc>>,
        next_id: Option<i64>,
        next_at: Option<DateTime<Utc>>,
        pending: bool,
    }
    let mut tx = pool.begin().await?;
    let Some(Resumed {
        rule,
        tz,
        until,
        next_id,
        next_at,
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
        && let Some(at) = JobSchedule::from_row(&rule, &tz, until)
            .map_err(|e| Error::InvalidParams(vec![e]))?
            .next(next_at.map_or(now, |at| at.max(now)))
    {
        clone_schedule_at(&mut *tx, next_id, at, instance_id).await?;
    }
    tx.commit().await?;
    Ok(true)
}

/// A schedule with its request and its last processed run.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ScheduleView {
    pub schedule_id: String,
    pub rule: String,
    pub tz: String,
    pub until: Option<DateTime<Utc>>,
    pub next_id: Option<i64>,
    pub next_at: Option<DateTime<Utc>>,
    pub paused: bool,
    #[sqlx(json(nullable))]
    pub meta: Option<JobMeta>,
    /// Its next run is waiting (scheduled or enqueued).
    pub pending: bool,
    pub last_id: Option<i64>,
    pub last_status: Option<String>,
    pub last_run_at: Option<DateTime<Utc>>,
}

pub async fn list_view(pool: &Pool<Postgres>, limit: i32) -> Result<Vec<ScheduleView>, Error> {
    const SQL: &str = "
    SELECT s.schedule_id, s.rule, s.tz, s.until, s.next_id,
        s.next_at, s.paused, j.meta,
        (sc.id IS NOT NULL OR e.id IS NOT NULL OR c.id IS NOT NULL) AS pending,
        lp.id AS last_id, lp.status AS last_status, lp.at AS last_run_at
    FROM schedules AS s
    LEFT JOIN jobs AS j ON j.id = s.next_id
    LEFT JOIN scheduled AS sc ON sc.id = s.next_id
    LEFT JOIN enqueued AS e ON e.id = s.next_id
    LEFT JOIN claimed AS c ON c.id = s.next_id
    LEFT JOIN LATERAL (
        SELECT p.id, p.status, p.at
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
