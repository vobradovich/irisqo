use axum::http::StatusCode;
use problemdetails::Problem;

// region:    Error
#[derive(thiserror::Error, Debug)]
pub enum Error {
    // #[error("Entity Not Found - {0}[{1}] ")]
    // EntityNotFound(&'static str, String),
    #[error(transparent)]
    DbError(#[from] sqlx::Error),

    #[error(transparent)]
    IoError(#[from] std::io::Error),
}

impl From<Error> for Problem {
    fn from(item: Error) -> Problem {
        match item {
            Error::DbError(sqlx::Error::RowNotFound) => problemdetails::new(StatusCode::NOT_FOUND)
                // .with_type("https://example.com/probs/out-of-credit")
                .with_title(StatusCode::NOT_FOUND.as_str())
                .with_detail(item.to_string()),
            _ => problemdetails::new(StatusCode::INTERNAL_SERVER_ERROR)
                // .with_type("https://example.com/probs/out-of-credit")
                .with_title(StatusCode::INTERNAL_SERVER_ERROR.as_str())
                .with_detail(item.to_string()),
        }
    }
}
