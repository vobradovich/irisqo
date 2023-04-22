use crate::{db, models::AppState};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use problemdetails::Problem;
use std::sync::Arc;

pub async fn delete(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> Result<StatusCode, Problem> {
    let _ = db::jobs::delete(&state.pool, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn get_by_id(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> Result<Response, Problem> {
    let job = db::jobs::get_by_id(&state.pool, id).await?;
    match job {
        None => Ok(StatusCode::NO_CONTENT.into_response()),
        Some(o) => Ok(Json(o).into_response()),
    }
}
