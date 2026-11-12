//! Read model of the jobs for the UI: a job's state is the table it is in
//! (`scheduled`, `enqueued`, `claimed`, `processed`), refined by its retry.

use chrono::{DateTime, Utc};
use sqlx::{Pool, Postgres};

use crate::models::{Error, JobMeta};

/// The state of a job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobStatus {
    Scheduled,
    Retrying,
    Enqueued,
    Processing,
    Completed,
    Failed,
    Cancelled,
    /// In none of the tables.
    Unknown,
}

impl JobStatus {
    /// Every state a job can be shown in.
    pub const KNOWN: [JobStatus; 7] = [
        JobStatus::Scheduled,
        JobStatus::Retrying,
        JobStatus::Enqueued,
        JobStatus::Processing,
        JobStatus::Completed,
        JobStatus::Failed,
        JobStatus::Cancelled,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            JobStatus::Scheduled => "scheduled",
            JobStatus::Retrying => "retrying",
            JobStatus::Enqueued => "enqueued",
            JobStatus::Processing => "processing",
            JobStatus::Completed => "completed",
            JobStatus::Failed => "failed",
            JobStatus::Cancelled => "cancelled",
            JobStatus::Unknown => "unknown",
        }
    }

    /// What the UI shows.
    pub const fn label(self) -> &'static str {
        match self {
            JobStatus::Scheduled => "Scheduled",
            JobStatus::Retrying => "Retrying",
            JobStatus::Enqueued => "Enqueued",
            JobStatus::Processing => "Processing",
            JobStatus::Completed => "Completed",
            JobStatus::Failed => "Failed",
            JobStatus::Cancelled => "Cancelled",
            JobStatus::Unknown => "Unknown",
        }
    }

    /// Waiting for its turn: not started yet.
    pub const fn is_pending(self) -> bool {
        matches!(
            self,
            JobStatus::Scheduled | JobStatus::Retrying | JobStatus::Enqueued
        )
    }

    pub const fn is_processed(self) -> bool {
        matches!(
            self,
            JobStatus::Completed | JobStatus::Failed | JobStatus::Cancelled
        )
    }
}

impl From<String> for JobStatus {
    fn from(value: String) -> Self {
        Self::KNOWN
            .into_iter()
            .find(|s| s.as_str() == value)
            .unwrap_or(JobStatus::Unknown)
    }
}

impl std::fmt::Display for JobStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Which jobs a list shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StatusFilter {
    /// scheduled, retrying, enqueued or processing
    #[default]
    Active,
    Only(JobStatus),
    All,
}

impl StatusFilter {
    pub const ALL: [StatusFilter; 9] = [
        StatusFilter::Active,
        StatusFilter::Only(JobStatus::Scheduled),
        StatusFilter::Only(JobStatus::Retrying),
        StatusFilter::Only(JobStatus::Enqueued),
        StatusFilter::Only(JobStatus::Processing),
        StatusFilter::Only(JobStatus::Completed),
        StatusFilter::Only(JobStatus::Failed),
        StatusFilter::Only(JobStatus::Cancelled),
        StatusFilter::All,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            StatusFilter::Active => "active",
            StatusFilter::Only(status) => status.as_str(),
            StatusFilter::All => "all",
        }
    }

    /// What the UI shows.
    pub const fn label(self) -> &'static str {
        match self {
            StatusFilter::Active => "Active",
            StatusFilter::Only(status) => status.label(),
            StatusFilter::All => "All",
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
    pub const ALL: [ScheduleFilter; 3] = [
        ScheduleFilter::Any,
        ScheduleFilter::Scheduled,
        ScheduleFilter::None,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            ScheduleFilter::Any => "any",
            ScheduleFilter::Scheduled => "scheduled",
            ScheduleFilter::None => "none",
        }
    }

    pub fn parse(value: &str) -> Self {
        Self::ALL
            .into_iter()
            .find(|f| f.as_str() == value)
            .unwrap_or_default()
    }
}

#[derive(Debug, Clone, Default)]
pub struct JobQuery {
    pub status: StatusFilter,
    pub schedule: ScheduleFilter,
    /// A job id, a key or a part of the URL (a number: the id or the key only).
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
    pub key: Option<String>,
    pub created_at: DateTime<Utc>,
    #[sqlx(try_from = "String")]
    pub status: JobStatus,
    pub attempt: i32,
    /// When a scheduled job is due.
    pub scheduled_at: Option<DateTime<Utc>>,
    /// Since when an enqueued job waits.
    pub enqueued_at: Option<DateTime<Utc>>,
    pub claimed_at: Option<DateTime<Utc>>,
    pub instance_id: Option<String>,
    pub processed_at: Option<DateTime<Utc>>,
}

const VIEW: &str = "
    SELECT j.id, j.meta, j.schedule_id, j.key, j.created_at,
        CASE
            WHEN s.id IS NOT NULL THEN CASE WHEN s.attempt > 0 THEN 'retrying' ELSE 'scheduled' END
            WHEN e.id IS NOT NULL THEN 'enqueued'
            WHEN c.id IS NOT NULL THEN 'processing'
            WHEN p.id IS NOT NULL THEN p.status
            ELSE 'unknown'
        END AS status,
        coalesce(s.attempt, e.attempt, c.attempt, p.attempt, 0) AS attempt,
        s.at AS scheduled_at, e.at AS enqueued_at, c.at AS claimed_at,
        c.instance_id::text AS instance_id, p.at AS processed_at
    FROM jobs AS j
    LEFT JOIN scheduled AS s ON s.id = j.id
    LEFT JOIN enqueued AS e ON e.id = j.id
    LEFT JOIN claimed AS c ON c.id = j.id
    LEFT JOIN processed AS p ON p.id = j.id";

/// `%` and `_` of a search term match themselves in `ILIKE`.
fn like_pattern(term: &str) -> String {
    let escaped = term
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    format!("%{escaped}%")
}

impl StatusFilter {
    /// The ids that can have this status, read from the state table(s) of it: a filter must not
    /// scan `jobs` (millions of finished rows) to find a few waiting ones. `None`: every job.
    const fn candidates(self) -> Option<&'static str> {
        Some(match self {
            StatusFilter::All | StatusFilter::Only(JobStatus::Unknown) => return None,
            StatusFilter::Active => {
                "SELECT id FROM scheduled UNION ALL SELECT id FROM enqueued UNION ALL SELECT id FROM claimed"
            }
            StatusFilter::Only(JobStatus::Scheduled) => {
                "SELECT id FROM scheduled WHERE attempt = 0"
            }
            StatusFilter::Only(JobStatus::Retrying) => "SELECT id FROM scheduled WHERE attempt > 0",
            StatusFilter::Only(JobStatus::Enqueued) => "SELECT id FROM enqueued",
            StatusFilter::Only(JobStatus::Processing) => "SELECT id FROM claimed",
            StatusFilter::Only(JobStatus::Completed) => {
                "SELECT id FROM processed WHERE status = 'completed'"
            }
            StatusFilter::Only(JobStatus::Failed) => {
                "SELECT id FROM processed WHERE status = 'failed'"
            }
            StatusFilter::Only(JobStatus::Cancelled) => {
                "SELECT id FROM processed WHERE status = 'cancelled'"
            }
        })
    }
}

/// Newest first, `query.limit` at most.
pub async fn list(pool: &Pool<Postgres>, query: &JobQuery) -> Result<Vec<JobView>, Error> {
    // start from the rows of the status asked for, then the same view as `get`
    let view = match query.status.candidates() {
        Some(ids) => VIEW.replacen(
            "FROM jobs AS j",
            &format!("FROM jobs AS j INNER JOIN ({ids}) AS cand ON cand.id = j.id"),
            1,
        ),
        None => VIEW.to_owned(),
    };
    let sql = format!(
        "WITH v AS ({view}
            WHERE ($1::bigint IS NULL OR j.id < $1)
              AND ($2::text IS NULL OR j.id::text = $2 OR j.key = $2 OR ($3::text IS NOT NULL AND j.meta->>'url' ILIKE $3))
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
        // a number is a job id (or a key), not a part of every URL with a port
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
        (SELECT count(*) FROM instances WHERE stopped_at IS NULL) AS instances
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
