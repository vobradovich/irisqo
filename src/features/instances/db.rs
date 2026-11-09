use crate::models::Error;
use serde::Serialize;
use sqlx::{Pool, Postgres};

/// Upsert an instance heartbeat, updating `seen_at` when it already exists.
pub async fn live(pool: &Pool<Postgres>, instance_id: &str, roles: &[&str]) -> Result<(), Error> {
    const SQL: &str = "
    INSERT INTO instances(instance_id, roles, version) VALUES ($1, $2, $3)
    ON CONFLICT (instance_id) DO UPDATE SET seen_at = now(), stopped_at = NULL, roles = $2
    ";
    sqlx::query(SQL)
        .bind(instance_id)
        .bind(roles)
        .bind(env!("CARGO_PKG_VERSION"))
        .execute(pool)
        .await?;
    Ok(())
}

/// Fetch a page of instances ordered by newest id first.
pub(crate) async fn get_all(
    pool: &Pool<Postgres>,
    limit: i32,
    offset: i32,
) -> Result<Vec<InstanceRow>, Error> {
    const SQL: &str = "
    SELECT instance_id, stopped_at, seen_at
    FROM instances ORDER BY instance_id DESC LIMIT $1 OFFSET $2
    ";
    let res = sqlx::query_as::<_, InstanceRow>(SQL)
        .bind(limit)
        .bind(offset)
        .fetch_all(pool)
        .await?;
    Ok(res)
}

/// Row entity for `instances` queries.
#[derive(Debug, Clone, sqlx::FromRow, Serialize)]
pub struct InstanceRow {
    pub instance_id: String,
    pub stopped_at: Option<chrono::DateTime<chrono::Utc>>,
    pub seen_at: chrono::DateTime<chrono::Utc>,
}

/// An instance with its work: jobs it holds now and its results of the last 24 hours.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct InstanceView {
    pub instance_id: String,
    pub version: String,
    pub roles: Vec<String>,
    pub stopped_at: Option<chrono::DateTime<chrono::Utc>>,
    pub seen_at: chrono::DateTime<chrono::Utc>,
    pub processing: i64,
    pub completed_24h: i64,
    pub failed_24h: i64,
}

/// Which instances a list shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InstanceFilter {
    /// Not stopped (including the ones that missed their heartbeat).
    #[default]
    Live,
    All,
}

impl InstanceFilter {
    pub const ALL: [InstanceFilter; 2] = [InstanceFilter::Live, InstanceFilter::All];

    pub const fn as_str(self) -> &'static str {
        match self {
            InstanceFilter::Live => "live",
            InstanceFilter::All => "all",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            InstanceFilter::Live => "Live",
            InstanceFilter::All => "All",
        }
    }

    pub fn parse(value: &str) -> Self {
        Self::ALL
            .into_iter()
            .find(|f| f.as_str() == value)
            .unwrap_or_default()
    }
}

/// Newest first (by start), `limit` at most; `before` is the last instance of the previous page.
pub async fn list_view(
    pool: &Pool<Postgres>,
    filter: InstanceFilter,
    before: Option<&str>,
    limit: i64,
) -> Result<Vec<InstanceView>, Error> {
    const SQL: &str = "
    WITH e AS (
        SELECT instance_id, count(*) AS processing
        FROM claimed GROUP BY instance_id
    ), p AS (
        SELECT instance_id,
            count(*) FILTER (WHERE status = 'completed') AS completed_24h,
            count(*) FILTER (WHERE status = 'failed') AS failed_24h
        FROM processed WHERE at > now() - interval '24 hours' GROUP BY instance_id
    )
    SELECT i.instance_id, i.version, i.roles, i.stopped_at, i.seen_at,
        coalesce(e.processing, 0) AS processing,
        coalesce(p.completed_24h, 0) AS completed_24h,
        coalesce(p.failed_24h, 0) AS failed_24h
    FROM instances i
    LEFT JOIN e USING (instance_id)
    LEFT JOIN p USING (instance_id)
    WHERE ($1 = 'all' OR i.stopped_at IS NULL)
      AND ($2::text IS NULL OR (i.started_at, i.instance_id) < (
            SELECT s.started_at, s.instance_id FROM instances AS s WHERE s.instance_id = $2))
    ORDER BY i.started_at DESC, i.instance_id DESC
    LIMIT $3";
    let rows = sqlx::query_as::<_, InstanceView>(SQL)
        .bind(filter.as_str())
        .bind(before)
        .bind(limit)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}
