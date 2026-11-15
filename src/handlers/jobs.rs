use super::{
    http::{create_job, job_headers, parse_body},
    job_request::JobRequest,
};
use crate::models::{Error, JobCreateRow, JobRow, Problem};
use crate::{db, db::jobqueue::ActionOutcome, models::AppState};
use axum::{
    Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use chrono::Utc;
use std::sync::Arc;

pub fn routes(state: Arc<AppState>) -> Router {
    Router::new()
        .route(
            "/jobs",
            post(create).layer(DefaultBodyLimit::max(state.max_body_size)),
        )
        .route("/jobs", get(list))
        .route("/jobs/{id}", get(get_by_id).delete(delete_by_id))
        .route("/jobs/{id}/cancel", post(cancel))
        .route("/jobs/{id}/retry", post(retry))
        .with_state(state)
}

/// Creates a job from JSON: `200` with the job (`id`, `key`) and the same in the headers
/// (`Location`, `Irisqo-*`). A repeated `key` returns the existing job.
#[utoipa::path(
    post,
    path = "/api/v1/jobs",
    operation_id = "create_job",
    tag = "jobs",
    request_body = JobRequest,
    responses(
        (status = 200, description = "The job, new or the existing one with the same key", body = JobCreateRow),
        (status = 400, description = "Invalid fields (RFC 9457 problem with `errors`)", body = Problem, content_type = "application/problem+json"),
    )
)]
pub(crate) async fn create(
    State(state): State<Arc<AppState>>,
    body: Bytes,
) -> Result<impl IntoResponse, Problem> {
    let job = parse_body::<JobRequest>(&body, "job")?
        .validate(&state.worker_options, Utc::now())
        .map_err(Error::InvalidParams)?;
    let job = create_job(&state, job).await?;
    Ok((job_headers(&job), Json(job)))
}

#[derive(serde::Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct JobsFilter {
    /// The job whose callbacks are listed.
    parent_id: i64,
    /// The id of the last job of the previous page: the page starts after it.
    after: Option<i64>,
    /// The size of a page: 100 when absent, 1000 at most.
    #[param(minimum = 1, maximum = 1000)]
    limit: Option<i64>,
}

/// The callbacks of a job (`parent_id`), oldest first, the jobs as `GET /jobs/{id}` gives them.
/// A page with fewer jobs than `limit` is the last one; otherwise the next page is the one
/// `after` the last job of this one.
#[utoipa::path(
    get,
    path = "/api/v1/jobs",
    operation_id = "list_jobs",
    tag = "jobs",
    params(JobsFilter),
    responses((status = 200, description = "The callbacks of the job", body = Vec<JobRow>))
)]
pub(crate) async fn list(
    State(state): State<Arc<AppState>>,
    Query(filter): Query<JobsFilter>,
) -> Result<Json<Vec<JobRow>>, Problem> {
    Ok(Json(
        db::jobqueue::list_by_parent(
            &state.pool,
            filter.parent_id,
            filter.after,
            filter.limit.unwrap_or(100).clamp(1, 1000),
        )
        .await?,
    ))
}

/// Deletes the job with its history and result; `204` also when there is no such job.
#[utoipa::path(
    delete,
    path = "/api/v1/jobs/{id}",
    operation_id = "delete_job",
    tag = "jobs",
    params(("id" = i64, Path, description = "The id of the job")),
    responses((status = 204, description = "Deleted"))
)]
pub(crate) async fn delete_by_id(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> Result<StatusCode, Problem> {
    let _ = db::jobqueue::delete(&state.pool, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// The job as it is stored; the values of the headers meant for the target are hidden.
#[utoipa::path(
    get,
    path = "/api/v1/jobs/{id}",
    operation_id = "get_job",
    tag = "jobs",
    params(("id" = i64, Path, description = "The id of the job")),
    responses(
        (status = 200, description = "The job", body = JobRow),
        (status = 404, description = "No such job", body = Problem, content_type = "application/problem+json"),
    )
)]
pub(crate) async fn get_by_id(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> Result<Response, Problem> {
    let job = db::jobqueue::get_by_id(&state.pool, id)
        .await?
        .ok_or(Error::JobNotFound(id))?;
    Ok(Json(job).into_response())
}

fn action_status(outcome: ActionOutcome) -> StatusCode {
    match outcome {
        ActionOutcome::Done => StatusCode::NO_CONTENT,
        ActionOutcome::Conflict => StatusCode::CONFLICT,
        ActionOutcome::NotFound => StatusCode::NOT_FOUND,
    }
}

/// Cancels a pending job; 409 when it is running or already processed.
#[utoipa::path(
    post,
    path = "/api/v1/jobs/{id}/cancel",
    operation_id = "cancel_job",
    tag = "jobs",
    params(("id" = i64, Path, description = "The id of the job")),
    responses(
        (status = 204, description = "Cancelled"),
        (status = 409, description = "The job is running or already processed"),
        (status = 404, description = "No such job"),
    )
)]
pub(crate) async fn cancel(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> Result<StatusCode, Problem> {
    let outcome = db::jobqueue::cancel_job(&state.pool, id, &state.instance_id).await?;
    Ok(action_status(outcome))
}

/// Runs a processed job once more; 409 when it is still pending or running.
#[utoipa::path(
    post,
    path = "/api/v1/jobs/{id}/retry",
    operation_id = "retry_job",
    tag = "jobs",
    params(("id" = i64, Path, description = "The id of the job")),
    responses(
        (status = 204, description = "Queued once more"),
        (status = 409, description = "The job is still pending or running"),
        (status = 404, description = "No such job"),
    )
)]
pub(crate) async fn retry(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> Result<StatusCode, Problem> {
    let outcome = db::jobqueue::requeue_job(&state.pool, id, &state.instance_id).await?;
    Ok(action_status(outcome))
}
