use chrono::{DateTime, Utc};
use serde::Serialize;

/// A declared flow: a row of `flows`.
#[derive(Debug, Clone, sqlx::FromRow, Serialize, utoipa::ToSchema)]
pub struct FlowRow {
    pub flow_id: String,
    /// Jobs of the flow running at once.
    pub concurrency: i32,
    /// Jobs started per `rate_period`, `null` without a limit.
    pub rate: Option<i32>,
    /// Whole seconds.
    pub rate_period: i32,
    pub paused: bool,
    /// When the rate limit lets the next job start.
    pub next_at: Option<DateTime<Utc>>,
}

/// A name that jobs use but nobody declared: it runs one job at a time. Only the names
/// with jobs in `enqueued` or `claimed` are listed.
#[derive(Debug, Clone, sqlx::FromRow, Serialize, utoipa::ToSchema)]
pub struct UndeclaredFlowRow {
    pub flow_id: String,
    /// Enqueued, waiting for a free place.
    pub ready: i64,
    /// Claimed by an instance.
    pub processing: i64,
}
