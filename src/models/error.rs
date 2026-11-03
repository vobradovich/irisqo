use axum::http::StatusCode;
use problemdetails::Problem;
use tokio::time::error::Elapsed;

use crate::{features::results::JobResult, otel};

/// A rejected request parameter (`url`, `_delay`, `_cron`...): the API returns them all
/// at once, the UI form shows each one next to its field.
#[derive(Debug, serde::Serialize)]
pub struct ParamError {
    pub param: &'static str,
    pub message: String,
}

impl ParamError {
    pub fn new(param: &'static str, message: impl Into<String>) -> Self {
        Self {
            param,
            message: message.into(),
        }
    }
}

fn describe(errors: &[ParamError]) -> String {
    errors
        .iter()
        .map(|e| format!("{}: {}", e.param, e.message))
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

    #[error("Invalid Url")]
    InvalidUrl,

    #[error("Invalid Params - {0}")]
    InvalidParams(&'static str),

    #[error("Invalid parameters: {}", describe(.0))]
    Validation(Vec<ParamError>),

    #[error("Server Error")]
    ServerError(Box<JobResult>),

    #[error("Client Error")]
    ClientError(Box<JobResult>),

    #[error("Response Too Large")]
    ResponseTooLarge,

    #[error("Retries Exceeded")]
    RetriesExceeded,
}

impl From<Error> for Problem {
    fn from(item: Error) -> Problem {
        let trace_id = otel::current_trace_id();
        match item {
            Error::InvalidUrl => problemdetails::new(StatusCode::BAD_REQUEST)
                // .with_type("https://example.com/probs/out-of-credit")
                .with_title(StatusCode::BAD_REQUEST.to_string())
                .with_detail(item.to_string())
                .with_value("trace_id", trace_id),
            Error::InvalidParams(_) | Error::Validation(_) => {
                let problem = problemdetails::new(StatusCode::BAD_REQUEST)
                    .with_title(StatusCode::BAD_REQUEST.to_string())
                    .with_detail(item.to_string())
                    .with_value("trace_id", trace_id);
                match item {
                    Error::Validation(errors) => problem
                        .with_value("errors", serde_json::to_value(errors).unwrap_or_default()),
                    _ => problem,
                }
            }
            Error::DbError(sqlx::Error::RowNotFound) => problemdetails::new(StatusCode::NOT_FOUND)
                // .with_type("https://example.com/probs/out-of-credit")
                .with_title(StatusCode::NOT_FOUND.to_string())
                .with_detail(item.to_string())
                .with_value("trace_id", trace_id),
            _ => problemdetails::new(StatusCode::INTERNAL_SERVER_ERROR)
                // .with_type("https://example.com/probs/out-of-credit")
                .with_title(StatusCode::INTERNAL_SERVER_ERROR.to_string())
                .with_detail(item.to_string())
                .with_instance(format!("{:?}", item))
                .with_value("trace_id", trace_id),
        }
    }
}
