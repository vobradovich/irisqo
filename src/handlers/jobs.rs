use crate::{db, models::AppState};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use problemdetails::Problem;
use std::sync::Arc;

pub fn routes(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/jobs/{id}", get(get_by_id).delete(delete_by_id))
        .with_state(state)
}

async fn delete_by_id(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> Result<StatusCode, Problem> {
    let _ = db::jobqueue::delete(&state.pool, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn get_by_id(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> Result<Response, Problem> {
    let job = db::jobqueue::get_by_id(&state.pool, id).await?;
    match job {
        None => Ok(StatusCode::NO_CONTENT.into_response()),
        Some(o) => Ok(Json(o).into_response()),
    }
}
