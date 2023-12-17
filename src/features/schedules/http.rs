use crate::{
    features::{Paging, PagingResult},
    models::AppState,
};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use problemdetails::Problem;
use std::sync::Arc;

pub fn routes(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/schedules/:id", get(get_by_id).delete(disable))
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
    let job_result = super::db::get_by_id(&state.pool, &schedule_id).await?;
    match job_result {
        None => Ok(StatusCode::NO_CONTENT.into_response()),
        Some(o) => Ok(Json(o).into_response()),
    }
}

async fn disable(
    State(state): State<Arc<AppState>>,
    Path(schedule_id): Path<String>,
) -> Result<Response, Problem> {
    let rows = super::db::disable(&state.pool, &schedule_id).await?;
    match rows {
        0 => Ok(StatusCode::NOT_FOUND.into_response()),
        _ => Ok(StatusCode::NO_CONTENT.into_response()),
    }
}
