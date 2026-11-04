use crate::models::AppState;
use crate::models::{Error, Problem};
use axum::{
    Json, Router,
    extract::{Path, State},
    response::{IntoResponse, Response},
    routing::get,
};
use std::sync::Arc;

use super::JobResult;

pub fn routes(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/jobs/{id}/result", get(result_by_id))
        .route("/jobs/{id}/result/raw", get(result_by_id_raw))
        .with_state(state)
}

async fn result_by_id(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> Result<Response, Problem> {
    let job_result = super::db::get_by_id(&state.pool, id)
        .await?
        .map(JobResult::from);
    let job_result = job_result.ok_or(Error::ResultNotFound(id))?;
    Ok(Json(job_result).into_response())
}

async fn result_by_id_raw(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> Result<Response, Problem> {
    let job_result = super::db::get_by_id(&state.pool, id)
        .await?
        .map(JobResult::from);
    let job_result = job_result.ok_or(Error::ResultNotFound(id))?;
    Ok(job_result.into_response())
}
