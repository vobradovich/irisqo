use std::collections::HashMap;

use axum::body::Bytes;

use serde::{Deserialize, Serialize};
use sqlx::types::Json;

use super::Error;

#[derive(Debug, Clone, Default)]
pub struct JobResult {
    pub meta: JobResultMeta,
    pub headers: Option<HashMap<String, String>>,
    pub body: Bytes,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct JobResultRow {
    pub id: i64,
    pub meta: Json<JobResultMeta>,
    pub headers: Option<Json<HashMap<String, String>>>,
    pub body: Bytes,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct JobResultMeta {
    #[serde(flatten, default)]
    pub result: JobResultType,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(tag = "result")]
#[serde(rename_all = "snake_case")]
pub enum JobResultType {
    #[default]
    Null,
    Timeout,
    Error {
        error: String,
    },
    Http(HttpResponseMeta),
    // Kafka,
    // Redis,
    // RabbitMq,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HttpResponseMeta {
    #[serde(with = "http_serde::status_code")]
    pub status_code: hyper::StatusCode,
    #[serde(with = "http_serde::version")]
    pub version: hyper::Version,
}

impl From<Error> for JobResult {
    fn from(value: Error) -> Self {
        let result = match value {
            Error::Timeout(_) => JobResultType::Timeout,
            _ => JobResultType::Error {
                error: value.to_string(),
            },
        };
        JobResult {
            meta: JobResultMeta { result },
            headers: None,
            body: Bytes::new(),
        }
    }
}
