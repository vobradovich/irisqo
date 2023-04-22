use crate::{db, models::AppState};
use axum::{extract::State, http::StatusCode, response::IntoResponse};
use problemdetails::Problem;
use std::sync::Arc;

pub async fn root() -> impl IntoResponse {
    (StatusCode::OK, "Use /jobs to get jobs.")
}

pub async fn live() -> impl IntoResponse {
    StatusCode::OK
}

pub async fn ready(State(state): State<Arc<AppState>>) -> Result<StatusCode, Problem> {
    db::select_one(&state.pool).await?;
    Ok(StatusCode::OK)
}
