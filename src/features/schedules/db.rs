use crate::{
    db,
    models::{Error, JobCreate, JobCreateRow, JobMeta, new_ulid},
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
/// the new run is a new job. Returns the run and whether the schedule itself is new.
pub async fn upsert(
    pool: &Pool<Postgres>,
    job: JobCreate,
    instance_id: &str,
) -> Result<(JobCreateRow, bool), Error> {
    // the row is created, or its rule is changed and it stays locked until the commit: two
    // declarations of a schedule follow each other; it gives the run that is waiting
    const SQL_SCHEDULE: &str = "
    INSERT INTO schedules(schedule_id, rule, tz, until, paused)
    VALUES ($1, $2, $3, $4::timestamptz, false)
    ON CONFLICT (schedule_id) DO UPDATE
        SET rule = EXCLUDED.rule, tz = EXCLUDED.tz,
            until = EXCLUDED.until, paused = false
    RETURNING next_id, (xmax = 0) AS created";
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
    // a run is always in the future: a `job.at` of the past counts as now
    let now = Utc::now();
    let Some(at) = schedule.next(job.at.map_or(now, |at| at.max(now))) else {
        return Err(Error::invalid(
            "until",
            "the schedule has no run before it ends",
        ));
    };
    let (schedule_id, rule, until) = (
        schedule_id.to_owned(),
        schedule.rule.to_string(),
        schedule.until,
    );

    let mut tx = pool.begin().await?;
    let (next_id, created) = sqlx::query_as::<_, (Option<i64>, bool)>(SQL_SCHEDULE)
        .bind(&schedule_id)
        .bind(&rule)
        .bind(JobSchedule::zone_name(&schedule.tz))
        .bind(until)
        .fetch_one(&mut *tx)
        .await?;
    if let Some(next_id) = next_id {
        let message = Some("replaced: the schedule was declared again");
        db::jobqueue::cancel(&mut tx, next_id, instance_id, message).await?;
    }
    let key = format!("{schedule_id}-{}", new_ulid());
    let (flow, priority) = (
        job.meta
            .flow_key
            .as_ref()
            .map(crate::models::FlowKey::as_str),
        job.meta.priority.unwrap_or(0),
    );
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
    Ok((
        JobCreateRow {
            id,
            schedule_id: Some(schedule_id),
            key: Some(key),
        },
        created,
    ))
}

/// Clones the job as the next run of its schedule.
/// Returns `None` when the schedule was deleted or deactivated in the meantime,
/// or when the job is not its latest run.
///
/// Locks the schedule (paused or not), reads `next_id` and `paused` under the lock; key `{schedule_id}-{ulid}`.
pub async fn clone_schedule_at<'e, E>(
    executor: E,
    schedule_id: &str,
    job_id: i64,
    at: DateTime<Utc>,
    instance_id: &str,
) -> Result<Option<i64>, Error>
where
    E: sqlx::Executor<'e, Database = Postgres>,
{
    const SQL: &str = "
    WITH s AS (
        -- locked even when paused: a resume in flight is waited for, not missed
        SELECT schedule_id, next_id, paused FROM schedules WHERE schedule_id = $5 FOR UPDATE
    ), a AS (
        INSERT INTO jobs(meta, headers, body, schedule_id, key)
        SELECT j.meta, j.headers, j.body, s.schedule_id, s.schedule_id || '-' || $4::text
        FROM s INNER JOIN jobs AS j ON j.id = s.next_id
        -- only the latest run continues the schedule (a manually retried old run does not)
        WHERE s.next_id = $1 AND NOT s.paused
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
        .bind(new_ulid())
        .bind(schedule_id)
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
        let message = Some("the schedule was paused");
        db::jobqueue::cancel(&mut tx, next_id, instance_id, message).await?;
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
    UPDATE schedules SET paused = false
    WHERE schedule_id = $1
    RETURNING rule, tz, until, next_id, next_at";
    // a statement of its own: a new snapshot, after a completion that held the lock
    const SQL_PENDING: &str = "
    SELECT EXISTS (SELECT 1 FROM scheduled WHERE id = $1)
        OR EXISTS (SELECT 1 FROM enqueued WHERE id = $1)
        OR EXISTS (SELECT 1 FROM claimed WHERE id = $1)";
    #[derive(sqlx::FromRow)]
    struct Resumed {
        rule: String,
        tz: String,
        until: Option<DateTime<Utc>>,
        next_id: Option<i64>,
        next_at: Option<DateTime<Utc>>,
    }
    let mut tx = pool.begin().await?;
    let Some(Resumed {
        rule,
        tz,
        until,
        next_id,
        next_at,
    }) = sqlx::query_as::<_, Resumed>(SQL)
        .bind(schedule_id)
        .fetch_optional(&mut *tx)
        .await?
    else {
        return Ok(false);
    };
    let Some(next_id) = next_id else {
        tx.commit().await?;
        return Ok(true);
    };
    let pending: bool = sqlx::query_scalar(SQL_PENDING)
        .bind(next_id)
        .fetch_one(&mut *tx)
        .await?;
    if !pending
        && let Some(at) = JobSchedule::from_row(&rule, &tz, until)
            .map_err(|e| Error::InvalidParams(vec![e]))?
            .next(next_at.map_or(now, |at| at.max(now)))
    {
        clone_schedule_at(&mut *tx, schedule_id, next_id, at, instance_id).await?;
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
    // the next statements see the run of a declaration or a clone that held the lock
    const SQL_LOCK: &str = "SELECT 1 FROM schedules WHERE schedule_id = $1 FOR UPDATE";
    const SQL: &str = "DELETE FROM schedules WHERE schedule_id = $1";
    let mut tx = pool.begin().await?;
    // not locked, not deleted: a first declaration in flight would keep its run
    if sqlx::query(SQL_LOCK)
        .bind(schedule_id)
        .fetch_optional(&mut *tx)
        .await?
        .is_none()
    {
        return Ok(0);
    }
    sqlx::query(SQL_NEXT)
        .bind(schedule_id)
        .execute(&mut *tx)
        .await?;
    let res = sqlx::query(SQL).bind(schedule_id).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(res.rows_affected())
}
