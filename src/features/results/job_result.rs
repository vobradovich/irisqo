use std::collections::HashMap;

use axum::response::IntoResponse;
use bytes::Bytes;

use hyper::{HeaderMap, StatusCode};
use serde::{Deserialize, Serialize};

use crate::models::{CallbackType, Error};

#[derive(Debug, Clone, Default, Serialize, utoipa::ToSchema)]
pub struct JobResult {
    pub meta: JobResultMeta,
    pub headers: Option<HashMap<String, String>>,
    /// The body of the response, base64 (`/result/raw` gives the bytes as they are).
    #[serde(serialize_with = "serialize_base64")]
    #[schema(value_type = String, format = Byte)]
    pub body: Bytes,
}

fn serialize_base64<S: serde::Serializer>(body: &Bytes, serializer: S) -> Result<S::Ok, S::Error> {
    use base64::Engine as _;
    serializer.serialize_str(&base64::engine::general_purpose::STANDARD.encode(body))
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default, utoipa::ToSchema)]
pub struct JobResultMeta {
    #[serde(flatten, default)]
    pub result: JobResultType,
    /// Trace and span of the attempt that produced the result.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub trace_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub span_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default, utoipa::ToSchema)]
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct HttpResponseMeta {
    #[serde(with = "http_serde::status_code")]
    #[schema(value_type = u16, example = 200)]
    pub status_code: hyper::StatusCode,
    #[serde(with = "http_serde::version")]
    #[schema(value_type = String, example = "HTTP/1.1")]
    pub version: hyper::Version,
    /// A failure that the status does not tell (the response is kept, the job is not done): the
    /// text of it, as `error` of a result that has no response.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub error: Option<String>,
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
                    error: None,
                }),
                trace_id: None,
                span_id: None,
            },
            headers,
            body,
        }
    }

    /// Marks an HTTP result as a failure of the job: the response is kept, the job is not done.
    #[must_use]
    pub(crate) fn failed_with(mut self, error: String) -> JobResult {
        if let JobResultType::Http(response) = &mut self.meta.result {
            response.error = Some(error);
        }
        self
    }

    /// Sets the trace and the span of the attempt that produced the result.
    pub(crate) fn traced(&mut self, ids: Option<(String, String)>) {
        if let Some((trace_id, span_id)) = ids {
            self.meta.trace_id = Some(trace_id);
            self.meta.span_id = Some(span_id);
        }
    }

    /// A job cancelled while it waited: no response, no trace.
    pub fn cancelled() -> JobResult {
        Self::with_type(JobResultType::Cancelled)
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

    /// How the job ended with this result: what its callback is of and `processed.status` is.
    pub fn callback_type(&self) -> CallbackType {
        match &self.meta.result {
            JobResultType::Timeout | JobResultType::Error { .. } => CallbackType::Failed,
            JobResultType::Cancelled => CallbackType::Cancelled,
            JobResultType::Http(meta) if meta.error.is_some() || !meta.status_code.is_success() => {
                CallbackType::Failed
            }
            JobResultType::None | JobResultType::Http(_) => CallbackType::Completed,
        }
    }

    /// `processed.status` for this result.
    pub fn processed_status(&self) -> &'static str {
        self.callback_type().status()
    }

    /// What the history says about the attempt: the error, or the HTTP status.
    pub fn history_message(&self) -> Option<String> {
        match &self.meta.result {
            JobResultType::Error { error } => Some(error.clone()),
            JobResultType::Timeout => Some("timeout".into()),
            JobResultType::Http(meta) => Some(match &meta.error {
                Some(error) => format!("HTTP {}: {error}", meta.status_code.as_u16()),
                None => format!("HTTP {}", meta.status_code.as_u16()),
            }),
            JobResultType::None | JobResultType::Cancelled => None,
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
            Error::ClientError(res) | Error::ServerError(res) => *res,
            Error::JobNotFound(_)
            | Error::ResultNotFound(_)
            | Error::ScheduleNotFound(_)
            | Error::FlowNotFound(_)
            | Error::DbError(_)
            | Error::IoError(_)
            | Error::HttpError(_)
            | Error::HyperError(_)
            | Error::HyperClientError(_)
            | Error::InvalidParams(_)
            | Error::BlockedTarget(_)
            | Error::RetriesExceeded => JobResult::with_type(JobResultType::Error {
                error: value.to_string(),
            }),
        }
    }
}

impl IntoResponse for JobResult {
    fn into_response(self) -> axum::response::Response {
        if let JobResultType::Http(meta) = self.meta.result {
            // the response is not what the job got: a bad gateway answers, not the target
            if meta.error.is_some() {
                return StatusCode::BAD_GATEWAY.into_response();
            }
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

#[test]
fn a_response_that_is_too_large_is_a_failure_with_its_status() {
    let result = JobResult::http(
        StatusCode::OK,
        hyper::Version::HTTP_11,
        Some(HashMap::from([("etag".into(), "x".into())])),
        Bytes::new(),
    );
    assert_eq!(result.processed_status(), "completed");
    let result = result.failed_with("response too large: 300 bytes, the limit is 100".to_owned());
    assert_eq!(result.processed_status(), "failed");
    assert_eq!(result.status_code(), Some(StatusCode::OK));
    assert_eq!(
        result.history_message().as_deref(),
        Some("HTTP 200: response too large: 300 bytes, the limit is 100")
    );
    assert_eq!(result.headers.as_ref().map(HashMap::len), Some(1));
    assert_eq!(
        result.into_response().status(),
        StatusCode::BAD_GATEWAY,
        "it is not the answer of the target"
    );

    let json = serde_json::to_value(JobResultMeta {
        result: JobResultType::Http(HttpResponseMeta {
            status_code: StatusCode::OK,
            version: hyper::Version::HTTP_11,
            error: Some("response too large: over the limit of 100 bytes".to_owned()),
        }),
        ..JobResultMeta::default()
    })
    .expect("json");
    // the same key and the same type as the `error` of a result that has no response
    assert_eq!(
        json["error"],
        "response too large: over the limit of 100 bytes"
    );
}
