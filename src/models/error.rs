use axum::http::StatusCode;
use problemdetails::Problem;
use tokio::time::error::Elapsed;

use crate::features::results::JobResult;

// region:    Error
#[derive(thiserror::Error, Debug)]
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

    #[error("Server Error")]
    ServerError(JobResult),

    #[error("Client Error")]
    ClientError(JobResult),
}

impl From<Error> for Problem {
    fn from(item: Error) -> Problem {
        match item {
            Error::InvalidUrl => problemdetails::new(StatusCode::BAD_REQUEST)
                // .with_type("https://example.com/probs/out-of-credit")
                .with_title(StatusCode::BAD_REQUEST.to_string())
                .with_detail(item.to_string()),
            Error::DbError(sqlx::Error::RowNotFound) => problemdetails::new(StatusCode::NOT_FOUND)
                // .with_type("https://example.com/probs/out-of-credit")
                .with_title(StatusCode::NOT_FOUND.to_string())
                .with_detail(item.to_string()),
            _ => problemdetails::new(StatusCode::INTERNAL_SERVER_ERROR)
                // .with_type("https://example.com/probs/out-of-credit")
                .with_title(StatusCode::INTERNAL_SERVER_ERROR.to_string())
                .with_detail(item.to_string())
                .with_instance(format!("{:?}", item)),
        }
    }
}
