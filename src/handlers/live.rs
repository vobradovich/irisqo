use crate::{db, models::AppState};
use axum::{extract::State, http::StatusCode, response::IntoResponse, Router, routing::get};
use problemdetails::Problem;
use std::sync::Arc;

pub fn routes(state: Arc<AppState>) -> Router {
	Router::new()
        .route("/", get(root))
        .route("/live", get(live))
        .route("/ready", get(ready))
        .with_state(state)
}

async fn root() -> impl IntoResponse {
    (StatusCode::OK, "Use /api/v1/jobs to get jobs.")
}

async fn live() -> impl IntoResponse {
    StatusCode::OK
}

async fn ready(State(state): State<Arc<AppState>>) -> Result<StatusCode, Problem> {
    db::select_one(&state.pool).await?;
    Ok(StatusCode::OK)
}