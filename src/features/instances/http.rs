use crate::features::Paging;
use crate::features::PagingResult;
use crate::models::AppState;
use axum::extract::Query;
use axum::Json;
use axum::{extract::State, response::IntoResponse, routing::get, Router};
use problemdetails::Problem;
use std::sync::Arc;

pub fn routes(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/instances", get(get_all))
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
