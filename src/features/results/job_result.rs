use std::collections::HashMap;

use axum::response::IntoResponse;
use bytes::Bytes;

use hyper::{HeaderMap, StatusCode};
use serde::{Deserialize, Serialize};

use crate::models::Error;

#[derive(Debug, Clone, Default, Serialize)]
pub struct JobResult {
    pub meta: JobResultMeta,
    pub headers: Option<HashMap<String, String>>,
    pub body: Bytes,
}

#[derive(sqlx::FromRow)]
pub struct JobResultRow {
    pub id: i64,
    #[sqlx(json)]
    pub meta: JobResultMeta,
    #[sqlx(json)]
    pub headers: Option<HashMap<String, String>>,
    pub body: Option<Vec<u8>>,
}

impl From<JobResultRow> for JobResult {
    fn from(value: JobResultRow) -> Self {
        JobResult {
            meta: value.meta,
            headers: value.headers,
            body: value.body.map_or(Bytes::new(), Bytes::from),
        }
    }
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
    None,
    Cancelled,
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

impl JobResult {
    pub(crate) fn http(
        status_code: hyper::StatusCode,
        version: hyper::Version,
        headers: Option<HashMap<String, String>>,
        body: Bytes,
    ) -> JobResult {
        JobResult {
            meta: JobResultMeta {
                result: JobResultType::Http(HttpResponseMeta {
                    status_code,
                    version,
                }),
            },
            headers,
            body,
        }
    }

    fn with_type(result: JobResultType) -> JobResult {
        JobResult {
            meta: JobResultMeta { result },
            headers: None,
            body: Bytes::new(),
        }
    }
}

impl From<Error> for JobResult {
    fn from(value: Error) -> Self {
        match value {
            Error::Timeout(_) => JobResult::with_type(JobResultType::Timeout),
            Error::ClientError(res) => res,
            Error::ServerError(res) => res,
            _ => JobResult::with_type(JobResultType::Error {
                error: value.to_string(),
            }),
        }
    }
}

impl IntoResponse for JobResult {
    fn into_response(self) -> axum::response::Response {
        if let JobResultType::Http(meta) = self.meta.result {
            let mut response = match self.body.is_empty() {
                true => meta.status_code.into_response(),
                false => (meta.status_code, self.body).into_response(),
            };
            if let Some(headers) = self.headers {
                let map = HeaderMap::try_from(&headers).unwrap();
                let headers_mut = response.headers_mut();
                for (key, value) in map {
                    if let Some(key) = key {
                        if key.to_string().starts_with("content") {
                            headers_mut.insert(key, value);
                        }
                    }
                }
            };
            return response;
        }
        StatusCode::NO_CONTENT.into_response()
    }
}
