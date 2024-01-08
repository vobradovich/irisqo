use crate::models::AppState;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use problemdetails::Problem;
use std::sync::Arc;

use super::JobResult;

pub fn routes(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/jobs/:id/result", get(result_by_id))
        .route("/jobs/:id/result/raw", get(result_by_id_raw))
        .with_state(state)
}

async fn result_by_id(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> Result<Response, Problem> {
    let job_result = super::db::get_by_id(&state.pool, id)
        .await?
        .map(JobResult::from);
    match job_result {
        None => Ok(StatusCode::NO_CONTENT.into_response()),
        Some(o) => Ok(Json(o).into_response()),
    }
}

async fn result_by_id_raw(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> Result<Response, Problem> {
    let job_result = super::db::get_by_id(&state.pool, id)
        .await?
        .map(JobResult::from);
    match job_result {
        None => Ok(StatusCode::NO_CONTENT.into_response()),
        Some(o) => Ok(o.into_response()),
    }
}
