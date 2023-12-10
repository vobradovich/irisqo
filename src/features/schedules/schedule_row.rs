use serde::Serialize;

#[derive(Debug, Clone, sqlx::FromRow, Serialize)]
pub struct ScheduleRow {
    pub schedule_id: String,
    pub schedule: String,
    pub until: Option<i64>,
    pub last_id: Option<i64>,
    pub last_at: Option<i64>,
    pub next_id: Option<i64>,
    pub next_at: Option<i64>,
    pub inactive: bool,
}
