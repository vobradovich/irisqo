use crate::models::{Error, Problem};
use crate::{
    features::{Paging, PagingResult},
    models::AppState,
};
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use std::sync::Arc;

pub fn routes(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/schedules/{id}", get(get_by_id).put(pause).delete(delete))
        .route("/schedules/{id}/pause", post(pause))
        .route("/schedules/{id}/resume", post(resume))
        .route("/schedules", get(get_all))
        .with_state(state)
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
        super::JobSchedule::now_secs(),
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
