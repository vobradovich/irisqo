use crate::models::AppState;
use crate::models::Error;
use crate::otel;
use axum::{extract::State, http::StatusCode, response::IntoResponse, routing::get, Router};
use problemdetails::Problem;
use sqlx::{Pool, Postgres};
use std::sync::Arc;

pub fn routes(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(root))
        .route("/live", get(live))
        .route("/ready", get(ready))
        .route("/error", get(error))
        .with_state(state)
}

async fn root() -> impl IntoResponse {
    (StatusCode::OK, "Use /to/* to queue a request.")
}

async fn live() -> impl IntoResponse {
    StatusCode::OK
}

async fn ready(State(state): State<Arc<AppState>>) -> Result<StatusCode, Problem> {
    select_one(&state.pool).await?;
    Ok(StatusCode::OK)
}

async fn error() -> impl IntoResponse {
    let trace_id = otel::current_trace_id();
    problemdetails::new(StatusCode::INTERNAL_SERVER_ERROR)
        // .with_type("https://example.com/probs/out-of-credit")
        .with_title(StatusCode::INTERNAL_SERVER_ERROR.to_string())
        .with_value("trace-id", trace_id)
}

async fn select_one(pool: &Pool<Postgres>) -> Result<(), Error> {
    _ = sqlx::query("SELECT 1").fetch_one(pool).await?;
    Ok(())
}
