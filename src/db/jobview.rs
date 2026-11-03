//! Read model of the jobs for the UI: a job's state is the table it is in
//! (`scheduled`, `enqueued`, `processed`), refined by its retry and its lock.

use chrono::{DateTime, Utc};
use sqlx::{Pool, Postgres};

use crate::models::{Error, JobMeta};

/// Which jobs a list shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StatusFilter {
    /// scheduled, retrying, enqueued or processing
    #[default]
    Active,
    Scheduled,
    Retrying,
    Enqueued,
    Processing,
    Completed,
    Failed,
    Cancelled,
    All,
}

impl StatusFilter {
    pub const ALL: [StatusFilter; 9] = [
        StatusFilter::Active,
        StatusFilter::Scheduled,
        StatusFilter::Retrying,
        StatusFilter::Enqueued,
        StatusFilter::Processing,
        StatusFilter::Completed,
        StatusFilter::Failed,
        StatusFilter::Cancelled,
        StatusFilter::All,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            StatusFilter::Active => "active",
            StatusFilter::Scheduled => "scheduled",
            StatusFilter::Retrying => "retrying",
            StatusFilter::Enqueued => "enqueued",
            StatusFilter::Processing => "processing",
            StatusFilter::Completed => "completed",
            StatusFilter::Failed => "failed",
            StatusFilter::Cancelled => "cancelled",
            StatusFilter::All => "all",
        }
    }

    pub fn parse(value: &str) -> Self {
        Self::ALL
            .into_iter()
            .find(|f| f.as_str() == value)
            .unwrap_or_default()
    }
}

/// Jobs with or without a schedule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScheduleFilter {
    #[default]
    Any,
    Scheduled,
    None,
}

impl ScheduleFilter {
    pub const fn as_str(self) -> &'static str {
        match self {
            ScheduleFilter::Any => "any",
            ScheduleFilter::Scheduled => "scheduled",
            ScheduleFilter::None => "none",
        }
    }

    pub fn parse(value: &str) -> Self {
        [
            ScheduleFilter::Any,
            ScheduleFilter::Scheduled,
            ScheduleFilter::None,
        ]
        .into_iter()
        .find(|f| f.as_str() == value)
        .unwrap_or_default()
    }
}

#[derive(Debug, Clone, Default)]
pub struct JobQuery {
    pub status: StatusFilter,
    pub schedule: ScheduleFilter,
    /// A job id, an external id or a part of the URL (a number: the id or the external id only).
    pub search: Option<String>,
    /// Keyset paging: jobs with a smaller id.
    pub before: Option<i64>,
    pub limit: i32,
}

/// A job with its state.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct JobView {
    pub id: i64,
    #[sqlx(json)]
    pub meta: JobMeta,
    pub schedule_id: Option<String>,
    pub external_id: Option<String>,
    pub created_at: DateTime<Utc>,
    /// scheduled | retrying | enqueued | processing | completed | failed | cancelled | unknown
    pub status: String,
    pub retry: i32,
    /// When a scheduled job is due, unix seconds.
    pub due_secs: Option<i64>,
    /// Since when an enqueued job waits.
    pub due_at: Option<DateTime<Utc>>,
    pub lock_at: Option<DateTime<Utc>>,
    pub instance_id: Option<String>,
    pub processed_at: Option<DateTime<Utc>>,
}

const VIEW: &str = "
    SELECT j.id, j.meta, j.schedule_id, j.external_id, j.created_at,
        CASE
            WHEN s.id IS NOT NULL THEN CASE WHEN s.retry > 0 THEN 'retrying' ELSE 'scheduled' END
            WHEN e.id IS NOT NULL THEN CASE WHEN e.lock_at IS NULL THEN 'enqueued' ELSE 'processing' END
            WHEN p.id IS NOT NULL THEN p.status::text
            ELSE 'unknown'
        END AS status,
        coalesce(s.retry, e.retry, p.retry, 0) AS retry,
        s.at AS due_secs, e.due_at, e.lock_at, e.instance_id, p.at AS processed_at
    FROM jobs AS j
    LEFT JOIN scheduled AS s ON s.id = j.id
    LEFT JOIN enqueued AS e ON e.id = j.id
    LEFT JOIN processed AS p ON p.id = j.id";

/// `%` and `_` of a search term match themselves in `ILIKE`.
fn like_pattern(term: &str) -> String {
    let escaped = term
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    format!("%{escaped}%")
}

/// Newest first, `query.limit` at most.
pub async fn list(pool: &Pool<Postgres>, query: &JobQuery) -> Result<Vec<JobView>, Error> {
    let sql = format!(
        "WITH v AS ({VIEW}
            WHERE ($1::bigint IS NULL OR j.id < $1)
              AND ($2::text IS NULL OR j.id::text = $2 OR j.external_id = $2 OR ($3::text IS NOT NULL AND j.meta->>'url' ILIKE $3))
              AND ($4 = 'any' OR ($4 = 'scheduled' AND j.schedule_id IS NOT NULL) OR ($4 = 'none' AND j.schedule_id IS NULL))
        )
        SELECT * FROM v
        WHERE $5 = 'all'
           OR ($5 = 'active' AND status IN ('scheduled', 'retrying', 'enqueued', 'processing'))
           OR status = $5
        ORDER BY id DESC
        LIMIT $6"
    );
    let search = query
        .search
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let rows = sqlx::query_as::<_, JobView>(sqlx::AssertSqlSafe(sql))
        .bind(query.before)
        .bind(search)
        // a number is a job id (or an external id), not a part of every URL with a port
        .bind(
            search
                .filter(|s| !s.bytes().all(|b| b.is_ascii_digit()))
                .map(like_pattern),
        )
        .bind(query.schedule.as_str())
        .bind(query.status.as_str())
        .bind(query.limit)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

pub async fn get(pool: &Pool<Postgres>, job_id: i64) -> Result<Option<JobView>, Error> {
    let sql = format!("{VIEW} WHERE j.id = $1");
    let row = sqlx::query_as::<_, JobView>(sqlx::AssertSqlSafe(sql))
        .bind(job_id)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

/// The last 24 hours and the instances, next to the queue stats.
#[derive(Debug, Clone, Copy, sqlx::FromRow)]
pub struct RecentStats {
    pub failed_24h: i64,
    pub processed_24h: i64,
    pub instances: i64,
}

pub async fn recent_stats(pool: &Pool<Postgres>) -> Result<RecentStats, Error> {
    const SQL: &str = "
    SELECT
        count(*) FILTER (WHERE status = 'failed') AS failed_24h,
        count(*) AS processed_24h,
        (SELECT count(*) FROM instances WHERE status = 'live') AS instances
    FROM processed WHERE at > now() - interval '24 hours'";
    let stats = sqlx::query_as::<_, RecentStats>(SQL)
        .fetch_one(pool)
        .await?;
    Ok(stats)
}

#[test]
fn like_pattern_escapes() {
    assert_eq!(like_pattern("50%_off"), "%50\\%\\_off%");
}
