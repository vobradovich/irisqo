use axum::http::StatusCode;
use tokio::time::error::Elapsed;

use super::Problem;
use crate::features::results::JobResult;

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
    #[error("Job Not Found - {0}")]
    JobNotFound(i64),

    /// No such job, or it is not processed yet.
    #[error("Result Not Found - {0}")]
    ResultNotFound(i64),

    #[error("Schedule Not Found - {0}")]
    ScheduleNotFound(String),

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

    /// The target address is not allowed (SSRF): never retried.
    #[error("Blocked target: {0}")]
    BlockedTarget(String),

    #[error("Retries Exceeded")]
    RetriesExceeded,
}

impl Error {
    /// One invalid parameter.
    pub fn invalid(parameter: &'static str, detail: impl Into<String>) -> Self {
        Error::InvalidParams(vec![ParamError::new(parameter, detail)])
    }

    /// A database error that may pass (lost connection, restart, deadlock): the write may be repeated.
    pub fn is_transient(&self) -> bool {
        match self {
            Error::DbError(
                sqlx::Error::Io(_) | sqlx::Error::PoolTimedOut | sqlx::Error::Tls(_),
            ) => true,
            // connection (08), deadlock and serialization (40), resources (53), shutdown (57)
            Error::DbError(sqlx::Error::Database(err)) => err
                .code()
                .is_some_and(|code| ["08", "40", "53", "57"].iter().any(|c| code.starts_with(c))),
            _ => false,
        }
    }
}

fn not_found(detail: String) -> Problem {
    Problem {
        detail: Some(detail),
        ..Problem::new(StatusCode::NOT_FOUND)
    }
}

/// RFC 9457 problem details. Only the invalid parameters are explained; any other
/// error is logged and answered with its status alone: no internals in the response.
impl From<Error> for Problem {
    fn from(item: Error) -> Problem {
        match item {
            Error::InvalidParams(errors) => {
                let detail = match errors.len() {
                    1 => "1 parameter is invalid.".to_owned(),
                    n => format!("{n} parameters are invalid."),
                };
                Problem {
                    kind: Some(INVALID_PARAMS_TYPE),
                    title: "Invalid parameters",
                    detail: Some(detail),
                    errors: Some(errors),
                    ..Problem::new(StatusCode::BAD_REQUEST)
                }
            }
            Error::JobNotFound(id) => not_found(format!("Job {id} does not exist.")),
            Error::ResultNotFound(id) => not_found(format!(
                "Job {id} has no result: it does not exist or is not processed yet."
            )),
            Error::ScheduleNotFound(id) => not_found(format!("Schedule {id} does not exist.")),
            Error::DbError(sqlx::Error::RowNotFound) => Problem::new(StatusCode::NOT_FOUND),
            Error::Timeout(_)
            | Error::DbError(_)
            | Error::IoError(_)
            | Error::HttpError(_)
            | Error::HyperError(_)
            | Error::HyperClientError(_)
            | Error::ServerError(_)
            | Error::ClientError(_)
            | Error::ResponseTooLarge
            | Error::BlockedTarget(_)
            | Error::RetriesExceeded => {
                let problem = Problem {
                    detail: Some(
                        "The request could not be processed, see the server logs.".to_owned(),
                    ),
                    ..Problem::new(StatusCode::INTERNAL_SERVER_ERROR)
                };
                tracing::error!(trace_id = problem.trace_id, "request failed: {:?}", item);
                problem
            }
        }
    }
}
