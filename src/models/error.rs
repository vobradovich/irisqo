use axum::http::StatusCode;
use problemdetails::Problem;

// region:    Error
#[derive(thiserror::Error, Debug)]
pub enum Error {
    // #[error("Entity Not Found - {0}[{1}] ")]
    // EntityNotFound(&'static str, String),

    #[error("Job Not Found - {0}")]
    JobNotFound(i64),

    #[error(transparent)]
    DbError(#[from] sqlx::Error),

    #[error(transparent)]
    IoError(#[from] std::io::Error),

    #[error(transparent)]
    HttpError(#[from] axum::http::Error),

    #[error("Invalid Url")]
    InvalidUrl,

    #[error("Invalid Params - {0}")]
    InvalidParams(&'static str),
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
                .with_detail(item.to_string()),
        }
    }
}
