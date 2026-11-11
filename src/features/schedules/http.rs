use super::ScheduleRow;
use super::schedule_request::prepare_schedule;
use crate::handlers::http::{create_job, job_headers, parse_body};
use crate::models::{Error, JobCreateRow, Problem};
use crate::{
    features::{Paging, PagingResult},
    models::AppState,
};
use axum::{
    Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, Path, Query, State},
    http::{HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use chrono::Utc;
use std::sync::Arc;

pub fn routes(state: Arc<AppState>) -> Router {
    Router::new()
        .route(
            "/schedules/{id}",
            get(get_by_id)
                .put(upsert)
                .delete(delete)
                .layer(DefaultBodyLimit::max(state.max_body_size)),
        )
        .route("/schedules/{id}/pause", post(pause))
        .route("/schedules/{id}/resume", post(resume))
        .route("/schedules", get(get_all))
        .with_state(state)
}

/// Declares a schedule: created with its first run, or changed (it keeps its id, the waiting run
/// is cancelled and replaced by one with the new job and rule; a paused schedule is resumed).
/// `200` with the run that is next (`id`, `schedule_id`, `key`) and the same in the headers.
#[utoipa::path(
    put,
    path = "/api/v1/schedules/{id}",
    operation_id = "put_schedule",
    tag = "schedules",
    params(("id" = String, Path, description = "The id of the schedule")),
    request_body = crate::handlers::openapi::ScheduleBody,
    responses(
        (status = 200, description = "The run that is next", body = JobCreateRow),
        (status = 400, description = "Invalid fields (RFC 9457 problem with `errors`)", body = Problem, content_type = "application/problem+json"),
    )
)]
pub(crate) async fn upsert(
    State(state): State<Arc<AppState>>,
    Path(schedule_id): Path<String>,
    body: Bytes,
) -> Result<impl IntoResponse, Problem> {
    // the job and the rule are two views of one body
    let (job, spec) = (
        parse_body(&body, "schedule")?,
        parse_body(&body, "schedule")?,
    );
    let job = prepare_schedule(&state.worker_options, &schedule_id, job, spec, Utc::now())
        .map_err(Error::InvalidParams)?;
    let run = create_job(&state, job).await?;
    let mut headers = job_headers(&run);
    // the schedule is what was declared
    if let Ok(location) = HeaderValue::from_str(&format!("/api/v1/schedules/{schedule_id}")) {
        headers.insert("location", location);
    }
    Ok((headers, Json(run)))
}

/// A page of the schedules, by id.
#[utoipa::path(
    get,
    path = "/api/v1/schedules",
    operation_id = "list_schedules",
    tag = "schedules",
    params(Paging),
    responses((status = 200, description = "A page of schedules", body = PagingResult<ScheduleRow>))
)]
pub(crate) async fn get_all(
    State(state): State<Arc<AppState>>,
    Query(pagination): Query<Paging>,
) -> Result<impl IntoResponse, Problem> {
    let result = super::db::get_all(
        &state.pool,
        pagination.limit.unwrap_or(10),
        pagination.offset.unwrap_or(0),
    )
    .await?;
    Ok(Json(PagingResult {
        limit: pagination.limit.unwrap_or(10),
        offset: pagination.offset.unwrap_or(0),
        data: result,
    }))
}

/// A schedule with its rule and the run that is next.
#[utoipa::path(
    get,
    path = "/api/v1/schedules/{id}",
    operation_id = "get_schedule",
    tag = "schedules",
    params(("id" = String, Path, description = "The id of the schedule")),
    responses(
        (status = 200, description = "The schedule", body = ScheduleRow),
        (status = 404, description = "No such schedule", body = Problem, content_type = "application/problem+json"),
    )
)]
pub(crate) async fn get_by_id(
    State(state): State<Arc<AppState>>,
    Path(schedule_id): Path<String>,
) -> Result<Response, Problem> {
    let schedule = super::db::get_by_id(&state.pool, &schedule_id)
        .await?
        .ok_or(Error::ScheduleNotFound(schedule_id))?;
    Ok(Json(schedule).into_response())
}

/// Pauses the schedule and cancels its pending run.
#[utoipa::path(
    post,
    path = "/api/v1/schedules/{id}/pause",
    operation_id = "pause_schedule",
    tag = "schedules",
    params(("id" = String, Path, description = "The id of the schedule")),
    responses(
        (status = 204, description = "Paused"),
        (status = 404, description = "No such schedule"),
    )
)]
pub(crate) async fn pause(
    State(state): State<Arc<AppState>>,
    Path(schedule_id): Path<String>,
) -> Result<Response, Problem> {
    let found = super::db::pause(&state.pool, &schedule_id, &state.instance_id).await?;
    Ok(found_status(found).into_response())
}

fn found_status(found: bool) -> StatusCode {
    if found {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::NOT_FOUND
    }
}

/// Resumes the schedule: its next run is scheduled when none is pending.
#[utoipa::path(
    post,
    path = "/api/v1/schedules/{id}/resume",
    operation_id = "resume_schedule",
    tag = "schedules",
    params(("id" = String, Path, description = "The id of the schedule")),
    responses(
        (status = 204, description = "Resumed"),
        (status = 404, description = "No such schedule"),
    )
)]
pub(crate) async fn resume(
    State(state): State<Arc<AppState>>,
    Path(schedule_id): Path<String>,
) -> Result<Response, Problem> {
    let found = super::db::resume(
        &state.pool,
        &schedule_id,
        chrono::Utc::now(),
        &state.instance_id,
    )
    .await?;
    Ok(found_status(found).into_response())
}

/// Deletes the schedule with its pending run (the runs that are done stay).
#[utoipa::path(
    delete,
    path = "/api/v1/schedules/{id}",
    operation_id = "delete_schedule",
    tag = "schedules",
    params(("id" = String, Path, description = "The id of the schedule")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 404, description = "No such schedule"),
    )
)]
pub(crate) async fn delete(
    State(state): State<Arc<AppState>>,
    Path(schedule_id): Path<String>,
) -> Result<Response, Problem> {
    let rows = super::db::delete(&state.pool, &schedule_id).await?;
    match rows {
        0 => Ok(StatusCode::NOT_FOUND.into_response()),
        _ => Ok(StatusCode::NO_CONTENT.into_response()),
    }
}
