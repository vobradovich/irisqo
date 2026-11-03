use axum::http::StatusCode;
use problemdetails::Problem;
use tokio::time::error::Elapsed;

use crate::{features::results::JobResult, otel};

/// The problem type of [`Error::InvalidParams`] (RFC 9457 `type`), documented there.
pub const INVALID_PARAMS_TYPE: &str =
    "https://github.com/vobradovich/irisqo/blob/main/docs/problems.md#invalid-parameters";

/// A rejected request parameter (`url`, `_delay`, `_cron`...): the API returns them all
/// at once (RFC 9457 `errors` extension), the UI form shows each one next to its field.
#[derive(Debug, serde::Serialize)]
pub struct ParamError {
    /// The query parameter (`_delay`) or `url`.
    pub parameter: &'static str,
    /// What is wrong, for a human: "must be a number of seconds".
    pub detail: String,
}

impl ParamError {
    pub fn new(parameter: &'static str, detail: impl Into<String>) -> Self {
        Self {
            parameter,
            detail: detail.into(),
        }
    }
}

fn describe(errors: &[ParamError]) -> String {
    errors
        .iter()
        .map(|e| format!("{}: {}", e.parameter, e.detail))
        .collect::<Vec<_>>()
        .join("; ")
}

// region:    Error
#[derive(thiserror::Error, Debug)]
#[allow(clippy::enum_variant_names)]
pub enum Error {
    // #[error("Entity Not Found - {0}[{1}] ")]
    // EntityNotFound(&'static str, String),
    #[error("Job Not Found - {0}")]
    JobNotFound(i64),

    #[error(transparent)]
    Timeout(#[from] Elapsed),

    #[error(transparent)]
    DbError(#[from] sqlx::Error),

    #[error(transparent)]
    IoError(#[from] std::io::Error),

    #[error(transparent)]
    HttpError(#[from] axum::http::Error),

    #[error(transparent)]
    HyperError(#[from] hyper::Error),

    #[error(transparent)]
    HyperClientError(#[from] hyper_util::client::legacy::Error),

    /// The request parameters are wrong: every one of them, for a 400.
    #[error("Invalid parameters: {}", describe(.0))]
    InvalidParams(Vec<ParamError>),

    #[error("Server Error")]
    ServerError(Box<JobResult>),

    #[error("Client Error")]
    ClientError(Box<JobResult>),

    #[error("Response Too Large")]
    ResponseTooLarge,

    #[error("Retries Exceeded")]
    RetriesExceeded,
}

impl Error {
    /// One invalid parameter.
    pub fn invalid(parameter: &'static str, detail: impl Into<String>) -> Self {
        Error::InvalidParams(vec![ParamError::new(parameter, detail)])
    }
}

/// RFC 9457 problem details. Only the invalid parameters are explained; any other
/// error is logged and answered with its status alone: no internals in the response.
impl From<Error> for Problem {
    fn from(item: Error) -> Problem {
        let trace_id = otel::current_trace_id();
        // `status` in the body too (advisory in RFC 9457, handy for clients and logs)
        let problem = |status: StatusCode| {
            problemdetails::new(status)
                .with_value("status", status.as_u16())
                .with_value("trace_id", trace_id.clone())
        };
        let generic = |status: StatusCode| {
            problem(status).with_title(status.canonical_reason().unwrap_or_default())
        };
        match item {
            Error::InvalidParams(errors) => {
                let detail = match errors.len() {
                    1 => "1 parameter is invalid.".to_owned(),
                    n => format!("{n} parameters are invalid."),
                };
                problem(StatusCode::BAD_REQUEST)
                    .with_type(INVALID_PARAMS_TYPE)
                    .with_title("Invalid parameters")
                    .with_detail(detail)
                    .with_value("errors", serde_json::to_value(errors).unwrap_or_default())
            }
            Error::JobNotFound(_) | Error::DbError(sqlx::Error::RowNotFound) => {
                generic(StatusCode::NOT_FOUND)
            }
            Error::Timeout(_)
            | Error::DbError(_)
            | Error::IoError(_)
            | Error::HttpError(_)
            | Error::HyperError(_)
            | Error::HyperClientError(_)
            | Error::ServerError(_)
            | Error::ClientError(_)
            | Error::ResponseTooLarge
            | Error::RetriesExceeded => {
                tracing::error!({ trace_id }, "request failed: {:?}", item);
                generic(StatusCode::INTERNAL_SERVER_ERROR)
                    .with_detail("The request could not be processed, see the server logs.")
            }
        }
    }
}
