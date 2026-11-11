use super::schedule_input::{ScheduleInput, prepare_schedule};
use crate::handlers::http::{create_job, job_headers, parse_body};
use crate::models::{Error, Problem};
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
async fn upsert(
    State(state): State<Arc<AppState>>,
    Path(schedule_id): Path<String>,
    body: Bytes,
) -> Result<impl IntoResponse, Problem> {
    let spec: ScheduleInput = parse_body(&body, "schedule")?;
    let job = prepare_schedule(&state.worker_options, &schedule_id, spec, Utc::now())
        .map_err(Error::InvalidParams)?;
    let run = create_job(&state, job).await?;
    let mut headers = job_headers(&run);
    // the schedule is what was declared
    if let Ok(location) = HeaderValue::from_str(&format!("/api/v1/schedules/{schedule_id}")) {
        headers.insert("location", location);
    }
    Ok((headers, Json(run)))
}

async fn get_all(
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

async fn get_by_id(
    State(state): State<Arc<AppState>>,
    Path(schedule_id): Path<String>,
) -> Result<Response, Problem> {
    let schedule = super::db::get_by_id(&state.pool, &schedule_id)
        .await?
        .ok_or(Error::ScheduleNotFound(schedule_id))?;
    Ok(Json(schedule).into_response())
}

/// Pauses the schedule and cancels its pending run (`PUT` is kept for compatibility).
async fn pause(
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
async fn resume(
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

async fn delete(
    State(state): State<Arc<AppState>>,
    Path(schedule_id): Path<String>,
) -> Result<Response, Problem> {
    let rows = super::db::delete(&state.pool, &schedule_id).await?;
    match rows {
        0 => Ok(StatusCode::NOT_FOUND.into_response()),
        _ => Ok(StatusCode::NO_CONTENT.into_response()),
    }
}
