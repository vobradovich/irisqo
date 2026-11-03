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
    #[sqlx(json(nullable))]
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
    /// Trace and span of the attempt that produced the result.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub trace_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub span_id: Option<String>,
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
                trace_id: None,
                span_id: None,
            },
            headers,
            body,
        }
    }

    fn with_type(result: JobResultType) -> JobResult {
        JobResult {
            meta: JobResultMeta {
                result,
                trace_id: None,
                span_id: None,
            },
            headers: None,
            body: Bytes::new(),
        }
    }
}

/// Statuses worth another attempt: timeouts, rate limits and server errors.
/// Any other 4xx means the request itself is wrong and fails the job at once.
pub fn is_retryable_status(status: StatusCode) -> bool {
    status.is_server_error()
        || matches!(
            status,
            StatusCode::REQUEST_TIMEOUT | StatusCode::TOO_EARLY | StatusCode::TOO_MANY_REQUESTS
        )
}

/// `Retry-After` in seconds from now: delay-seconds or an HTTP-date.
pub fn parse_retry_after(value: &str, now_secs: i64) -> Option<u32> {
    let value = value.trim();
    if let Ok(secs) = value.parse::<u32>() {
        return Some(secs);
    }
    let at = chrono::DateTime::parse_from_rfc2822(value)
        .ok()?
        .timestamp();
    Some(u32::try_from(at.saturating_sub(now_secs).max(0)).unwrap_or(u32::MAX))
}

impl JobResult {
    /// HTTP status of the response, if the job got one.
    pub fn status_code(&self) -> Option<StatusCode> {
        match &self.meta.result {
            JobResultType::Http(meta) => Some(meta.status_code),
            JobResultType::None
            | JobResultType::Cancelled
            | JobResultType::Timeout
            | JobResultType::Error { .. } => None,
        }
    }

    /// `processed.status` for this result.
    pub fn processed_status(&self) -> &'static str {
        match &self.meta.result {
            JobResultType::Timeout | JobResultType::Error { .. } => "failed",
            JobResultType::Cancelled => "cancelled",
            JobResultType::Http(meta)
                if meta.status_code.is_client_error() || meta.status_code.is_server_error() =>
            {
                "failed"
            }
            JobResultType::None | JobResultType::Http(_) => "completed",
        }
    }

    /// `Retry-After` of the response in seconds from now.
    pub fn retry_after(&self, now_secs: i64) -> Option<u32> {
        let headers = self.headers.as_ref()?;
        let value = headers.get(hyper::header::RETRY_AFTER.as_str())?;
        parse_retry_after(value, now_secs)
    }
}

impl From<Error> for JobResult {
    fn from(value: Error) -> Self {
        match value {
            Error::Timeout(_) => JobResult::with_type(JobResultType::Timeout),
            Error::ClientError(res) | Error::ServerError(res) => res,
            Error::JobNotFound(_)
            | Error::DbError(_)
            | Error::IoError(_)
            | Error::HttpError(_)
            | Error::HyperError(_)
            | Error::HyperClientError(_)
            | Error::InvalidUrl
            | Error::InvalidParams(_)
            | Error::ResponseTooLarge
            | Error::RetriesExceeded => JobResult::with_type(JobResultType::Error {
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
            if let Some(headers) = self.headers
                && let Ok(map) = HeaderMap::try_from(&headers)
            {
                let headers_mut = response.headers_mut();
                for (key, value) in map {
                    if let Some(key) = key
                        && key.as_str().starts_with("content")
                    {
                        headers_mut.insert(key, value);
                    }
                }
            };
            return response;
        }
        StatusCode::NO_CONTENT.into_response()
    }
}

#[test]
fn retryable_statuses() {
    for code in [408, 425, 429, 500, 502, 503, 504] {
        assert!(
            is_retryable_status(StatusCode::from_u16(code).unwrap()),
            "{code}"
        );
    }
    for code in [400, 401, 403, 404, 409, 422] {
        assert!(
            !is_retryable_status(StatusCode::from_u16(code).unwrap()),
            "{code}"
        );
    }
}

#[test]
fn retry_after_parse() {
    // Wed, 21 Oct 2015 07:28:00 GMT
    let at = 1_445_412_480;
    assert_eq!(parse_retry_after("120", at), Some(120));
    assert_eq!(parse_retry_after(" 0 ", at), Some(0));
    assert_eq!(
        parse_retry_after("Wed, 21 Oct 2015 07:28:00 GMT", at - 60),
        Some(60)
    );
    assert_eq!(
        parse_retry_after("Wed, 21 Oct 2015 07:28:00 GMT", at + 60),
        Some(0)
    );
    assert_eq!(parse_retry_after("soon", at), None);
    assert_eq!(parse_retry_after("-5", at), None);
}
