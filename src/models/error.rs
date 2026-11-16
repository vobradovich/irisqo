use std::{borrow::Cow, collections::HashMap};

use axum::http::StatusCode;
use tokio::time::error::Elapsed;

use super::Problem;
use crate::features::results::JobResult;

/// The problem type of [`Error::InvalidParams`] (RFC 9457 `type`), documented there.
pub const INVALID_PARAMS_TYPE: &str =
    "https://github.com/vobradovich/irisqo/blob/main/docs/problems.md#invalid-parameters";

/// A rejected request parameter (`url`, `_delay`, `_cron`...): the API returns them all
/// at once (RFC 9457 `errors` extension), the UI form shows each one next to its field.
#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct ParamError {
    /// The query parameter (`_delay`) or `url`.
    #[schema(value_type = String)]
    pub parameter: Cow<'static, str>,
    /// What is wrong, for a human: "must be a number of seconds".
    pub detail: String,
}

impl ParamError {
    pub fn new(parameter: impl Into<Cow<'static, str>>, detail: impl Into<String>) -> Self {
        Self {
            parameter: parameter.into(),
            detail: detail.into(),
        }
    }
}

/// What is wrong with a timeout that does not parse.
pub const MILLISECONDS_RULE: &str = "must be a positive number of milliseconds";

/// What is wrong with a priority that does not parse.
pub const PRIORITY_RULE: &str = "must be an integer from -2147483648 to 2147483647";

/// Parameters as text by name (`/to/` without the `_` of its parameters): each one is parsed
/// on its own, and each one that does not parse is an error, named `prefix` and its name.
pub struct Params<'a> {
    values: &'a HashMap<String, String>,
    prefix: &'static str,
    errors: Vec<ParamError>,
}

impl<'a> Params<'a> {
    pub fn new(values: &'a HashMap<String, String>, prefix: &'static str) -> Self {
        Self {
            values,
            prefix,
            errors: Vec::new(),
        }
    }

    /// The parameter `name` as it is.
    pub fn raw(&self, name: &str) -> Option<&'a str> {
        self.values.get(name).map(String::as_str)
    }

    /// The parameter `name` parsed; `None` when it is absent, or when it does not parse, with an
    /// error that says `rule`.
    pub fn parse<T>(
        &mut self,
        name: &'static str,
        rule: &str,
        parse: impl FnOnce(&str) -> Option<T>,
    ) -> Option<T> {
        let parsed = parse(self.raw(name)?);
        if parsed.is_none() {
            self.error(name, rule);
        }
        parsed
    }

    pub fn error(&mut self, name: &'static str, detail: impl Into<String>) {
        let parameter = match self.prefix {
            "" => Cow::Borrowed(name),
            prefix => Cow::Owned(format!("{prefix}{name}")),
        };
        self.errors.push(ParamError::new(parameter, detail));
    }

    /// The errors of the parameters that did not parse.
    pub fn into_errors(self) -> Vec<ParamError> {
        self.errors
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

    #[error("Flow Not Found - {0}")]
    FlowNotFound(String),

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

    /// The target address is not allowed (SSRF): never retried.
    #[error("Blocked target: {0}")]
    BlockedTarget(String),

    #[error("Retries Exceeded")]
    RetriesExceeded,
}

impl Error {
    /// One invalid parameter.
    pub fn invalid(parameter: impl Into<Cow<'static, str>>, detail: impl Into<String>) -> Self {
        Error::InvalidParams(vec![ParamError::new(parameter, detail)])
    }

    /// A database error that may pass (lost connection, restart, deadlock): the write may be repeated.
    pub fn is_transient(&self) -> bool {
        match self {
            Error::DbError(err) => match err {
                sqlx::Error::Io(_) | sqlx::Error::PoolTimedOut | sqlx::Error::Tls(_) => true,
                // connection (08), deadlock and serialization (40), resources (53), shutdown (57)
                sqlx::Error::Database(err) => err.code().is_some_and(|code| {
                    ["08", "40", "53", "57"].iter().any(|c| code.starts_with(c))
                }),
                // sqlx::Error is non-exhaustive
                _ => false,
            },
            Error::JobNotFound(_)
            | Error::ResultNotFound(_)
            | Error::ScheduleNotFound(_)
            | Error::FlowNotFound(_)
            | Error::Timeout(_)
            | Error::IoError(_)
            | Error::HttpError(_)
            | Error::HyperError(_)
            | Error::HyperClientError(_)
            | Error::InvalidParams(_)
            | Error::ServerError(_)
            | Error::ClientError(_)
            | Error::BlockedTarget(_)
            | Error::RetriesExceeded => false,
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
            Error::FlowNotFound(name) => not_found(format!("Flow {name} does not exist.")),
            Error::DbError(sqlx::Error::RowNotFound) => Problem::new(StatusCode::NOT_FOUND),
            Error::Timeout(_)
            | Error::DbError(_)
            | Error::IoError(_)
            | Error::HttpError(_)
            | Error::HyperError(_)
            | Error::HyperClientError(_)
            | Error::ServerError(_)
            | Error::ClientError(_)
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
