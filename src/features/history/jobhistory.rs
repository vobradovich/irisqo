#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct JobHistoryRow {
    pub id: i64,
    pub retry: i32,
    pub instance_id: String,
    pub at: chrono::DateTime<chrono::Utc>,
    pub status: String,
    pub message: Option<String>,
}
