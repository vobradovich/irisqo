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

/// A declared flow of the list, with its jobs counted.
#[derive(Debug, Clone, sqlx::FromRow, Serialize, utoipa::ToSchema)]
pub struct FlowListRow {
    #[sqlx(flatten)]
    #[serde(flatten)]
    pub flow: FlowRow,
    /// Enqueued, waiting for a free place; counted up to 101.
    pub waiting: i64,
    /// Claimed by an instance.
    pub processing: i64,
    /// Starts the rate limit lets through right now (`rate` when idle, one more every
    /// `rate_period / rate`); `null` without a rate.
    pub rate_left: Option<i32>,
}

/// A name that jobs use but nobody declared: it runs one job at a time. Only the names
/// with jobs in `enqueued` or `claimed` are listed.
#[derive(Debug, Clone, sqlx::FromRow, Serialize, utoipa::ToSchema)]
pub struct UndeclaredFlowRow {
    pub flow_id: String,
    /// Enqueued, waiting for a free place; counted up to 101.
    pub waiting: i64,
    /// Claimed by an instance.
    pub processing: i64,
}
