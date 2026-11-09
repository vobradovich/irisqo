use chrono::{DateTime, Utc};
use serde::Serialize;

#[derive(Debug, Clone, sqlx::FromRow, Serialize)]
pub struct ScheduleRow {
    pub schedule_id: String,
    pub schedule: String,
    pub until: Option<DateTime<Utc>>,
    pub next_id: Option<i64>,
    pub next_at: Option<DateTime<Utc>>,
    pub paused: bool,
}
