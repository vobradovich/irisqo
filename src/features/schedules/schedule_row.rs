use chrono::{DateTime, Utc};
use serde::Serialize;

#[derive(Debug, Clone, sqlx::FromRow, Serialize, utoipa::ToSchema)]
pub struct ScheduleRow {
    pub schedule_id: String,
    pub rule: String,
    /// The IANA zone of a cron schedule.
    pub tz: String,
    pub until: Option<DateTime<Utc>>,
    pub next_id: Option<i64>,
    pub next_at: Option<DateTime<Utc>>,
    pub paused: bool,
}
