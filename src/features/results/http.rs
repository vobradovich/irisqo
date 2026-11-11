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

/// The result of the last attempt of a job: how it ended, the response headers and body.
#[utoipa::path(
    get,
    path = "/api/v1/jobs/{id}/result",
    operation_id = "get_job_result",
    tag = "jobs",
    params(("id" = i64, Path, description = "The id of the job")),
    responses(
        (status = 200, description = "The result", body = JobResult),
        (status = 404, description = "No result (the job is not processed yet)", body = Problem, content_type = "application/problem+json"),
    )
)]
pub(crate) async fn result_by_id(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> Result<Response, Problem> {
    let job_result = super::db::get_by_id(&state.pool, id)
        .await?
        .map(JobResult::from);
    let job_result = job_result.ok_or(Error::ResultNotFound(id))?;
    Ok(Json(job_result).into_response())
}

/// The response of the target as it came: its status, the body and the `content-*` headers;
/// `204` when the job did not get an HTTP response.
#[utoipa::path(
    get,
    path = "/api/v1/jobs/{id}/result/raw",
    operation_id = "get_job_result_raw",
    tag = "jobs",
    params(("id" = i64, Path, description = "The id of the job")),
    responses(
        (status = "default", description = "The status of the target's response, its body and `content-*` headers", content_type = "*/*"),
        (status = 204, description = "The job did not get an HTTP response"),
        (status = 404, description = "No result (the job is not processed yet)", body = Problem, content_type = "application/problem+json"),
    )
)]
pub(crate) async fn result_by_id_raw(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> Result<Response, Problem> {
    let job_result = super::db::get_by_id(&state.pool, id)
        .await?
        .map(JobResult::from);
    let job_result = job_result.ok_or(Error::ResultNotFound(id))?;
    Ok(job_result.into_response())
}
