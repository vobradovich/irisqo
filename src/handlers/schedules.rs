use crate::{db, models::AppState};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use problemdetails::Problem;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

pub fn routes(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/schedules/:id", get(get_by_id))
        .route("/schedules", get(get_all))
        .with_state(state)
}

async fn get_all(
    State(state): State<Arc<AppState>>,
    Query(pagination): Query<Pagination>,
) -> Result<impl IntoResponse, Problem> {
    let result =
        db::schedules::get_all(&state.pool, pagination.limit.unwrap_or(10), pagination.offset.unwrap_or(0)).await?;
    Ok(Json(PaginationResult { limit: pagination.limit.unwrap_or(10), offset: pagination.offset.unwrap_or(0), result }))
}

async fn get_by_id(
    State(state): State<Arc<AppState>>,
    Path(schedule_id): Path<String>,
) -> Result<Response, Problem> {
    let job_result = db::schedules::get_by_id(&state.pool, &schedule_id).await?;
    match job_result {
        None => Ok(StatusCode::NO_CONTENT.into_response()),
        Some(o) => Ok(Json(o).into_response()),
    }
}

#[derive(Deserialize)]
struct Pagination {
    limit: Option<i32>,
    offset: Option<i32>,
}

#[derive(Serialize)]
struct PaginationResult<T> {
    limit: i32,
    offset: i32,
    result: Vec<T>,
}