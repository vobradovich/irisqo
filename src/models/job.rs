use std::time::SystemTime;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub enum Protocol {
    Null,
    Http,
    Kafka,
    Redis,
    RabbitMq
}

#[derive(Debug, Clone, sqlx::FromRow, Serialize)]
pub struct JobRow {
    pub id: i64,
    pub protocol: String,
    pub url: String,
    // pub meta: HashMap<String, String>,
    // pub headers: Option<HashMap<String, String>>,
    pub body: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct JobCreate {
    pub protocol: String,
    pub url: String,
    // pub meta: HashMap<String, String>,
    // pub headers: Option<HashMap<String, String>>,
    pub body: Option<String>,
    pub at: Option<SystemTime>,
}

#[derive(Debug, sqlx::FromRow)]
pub struct JobQueueEntry {
    pub id: i64,
    pub retry: i32,
}
