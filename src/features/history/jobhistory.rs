#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct JobHistoryRow {
    pub id: i64,
    pub retry: i32,
    pub instance_id: String,
    pub at: chrono::DateTime<chrono::Utc>,
    #[sqlx(try_from = "String")]
    pub status: HistoryStatus,
    pub message: Option<String>,
}

/// A step in the history of a job (`history_status` in the database).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum HistoryStatus {
    Scheduled,
    Enqueued,
    Assigned,
    Retried,
    Completed,
    Failed,
    Cancelled,
    /// Not a known step: a newer database.
    Unknown,
}

impl HistoryStatus {
    pub const KNOWN: [HistoryStatus; 7] = [
        HistoryStatus::Scheduled,
        HistoryStatus::Enqueued,
        HistoryStatus::Assigned,
        HistoryStatus::Retried,
        HistoryStatus::Completed,
        HistoryStatus::Failed,
        HistoryStatus::Cancelled,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            HistoryStatus::Scheduled => "scheduled",
            HistoryStatus::Enqueued => "enqueued",
            HistoryStatus::Assigned => "assigned",
            HistoryStatus::Retried => "retried",
            HistoryStatus::Completed => "completed",
            HistoryStatus::Failed => "failed",
            HistoryStatus::Cancelled => "cancelled",
            HistoryStatus::Unknown => "unknown",
        }
    }

    /// What the UI shows.
    pub const fn label(self) -> &'static str {
        match self {
            HistoryStatus::Scheduled => "Scheduled",
            HistoryStatus::Enqueued => "Enqueued",
            HistoryStatus::Assigned => "Assigned",
            HistoryStatus::Retried => "Retried",
            HistoryStatus::Completed => "Completed",
            HistoryStatus::Failed => "Failed",
            HistoryStatus::Cancelled => "Cancelled",
            HistoryStatus::Unknown => "Unknown",
        }
    }
}

impl From<String> for HistoryStatus {
    fn from(value: String) -> Self {
        Self::KNOWN
            .into_iter()
            .find(|s| s.as_str() == value)
            .unwrap_or(HistoryStatus::Unknown)
    }
}

impl std::fmt::Display for HistoryStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}
