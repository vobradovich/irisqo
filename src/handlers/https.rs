use crate::{db, models::AppState};
use axum::{
    extract::{Path, State},
    http::StatusCode,
};
use problemdetails::Problem;
use std::sync::Arc;

pub async fn create(
    Path(url): Path<String>,
    State(state): State<Arc<AppState>>,
) -> Result<StatusCode, Problem> {
    let _ = db::jobs::enqueue(&state.pool, "https", &url).await?;
    Ok(StatusCode::CREATED)
}
