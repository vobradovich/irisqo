use chrono::{DateTime, Utc};
use serde::Serialize;

/// A declared queue: a row of `queues`.
#[derive(Debug, Clone, sqlx::FromRow, Serialize, utoipa::ToSchema)]
pub struct QueueRow {
    pub queue_id: String,
    /// Jobs of the queue running at once.
    pub parallelism: i32,
    /// `true`: jobs go in id order, a delayed retry holds the ones behind it.
    pub ordered: bool,
    /// Jobs started per `rate_period`, `null` without a limit.
    pub rate: Option<i32>,
    /// Whole seconds.
    pub rate_period: i64,
    pub paused: bool,
    /// When the rate limit lets the next job start.
    pub next_at: Option<DateTime<Utc>>,
}

/// A name that jobs use but nobody declared: it runs one job at a time, in order. Only the names
/// with jobs in `enqueued` or `claimed` are listed.
#[derive(Debug, Clone, sqlx::FromRow, Serialize, utoipa::ToSchema)]
pub struct UndeclaredQueueRow {
    pub queue_id: String,
    /// Enqueued and due.
    pub ready: i64,
    /// Enqueued for later: a retry waiting for its time.
    pub delayed: i64,
    /// Claimed by an instance.
    pub processing: i64,
}
