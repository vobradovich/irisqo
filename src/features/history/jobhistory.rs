#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct JobHistoryRow {
    pub id: i64,
    pub retry: i32,
    pub instance_id: String,
    pub at: chrono::DateTime<chrono::Utc>,
    #[sqlx(try_from = "String")]
    pub kind: HistoryKind,
    pub message: Option<String>,
}

/// A step in the history of a job (`history.kind` in the database).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum HistoryKind {
    Scheduled,
    Enqueued,
    Claimed,
    Released,
    Retried,
    Completed,
    Failed,
    Cancelled,
    Callback,
    /// Not a known step: a newer database.
    Unknown,
}

impl HistoryKind {
    pub const KNOWN: [HistoryKind; 9] = [
        HistoryKind::Scheduled,
        HistoryKind::Enqueued,
        HistoryKind::Claimed,
        HistoryKind::Released,
        HistoryKind::Retried,
        HistoryKind::Completed,
        HistoryKind::Failed,
        HistoryKind::Cancelled,
        HistoryKind::Callback,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            HistoryKind::Scheduled => "scheduled",
            HistoryKind::Enqueued => "enqueued",
            HistoryKind::Claimed => "claimed",
            HistoryKind::Released => "released",
            HistoryKind::Retried => "retried",
            HistoryKind::Completed => "completed",
            HistoryKind::Failed => "failed",
            HistoryKind::Cancelled => "cancelled",
            HistoryKind::Callback => "callback",
            HistoryKind::Unknown => "unknown",
        }
    }

    /// What the UI shows.
    pub const fn label(self) -> &'static str {
        match self {
            HistoryKind::Scheduled => "Scheduled",
            HistoryKind::Enqueued => "Enqueued",
            HistoryKind::Claimed => "Claimed",
            HistoryKind::Released => "Released",
            HistoryKind::Retried => "Retried",
            HistoryKind::Completed => "Completed",
            HistoryKind::Failed => "Failed",
            HistoryKind::Cancelled => "Cancelled",
            HistoryKind::Callback => "Callback",
            HistoryKind::Unknown => "Unknown",
        }
    }
}

impl From<String> for HistoryKind {
    fn from(value: String) -> Self {
        Self::KNOWN
            .into_iter()
            .find(|s| s.as_str() == value)
            .unwrap_or(HistoryKind::Unknown)
    }
}

impl std::fmt::Display for HistoryKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}
