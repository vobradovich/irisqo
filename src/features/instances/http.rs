use super::db::InstanceRow;
use crate::features::Paging;
use crate::features::PagingResult;
use crate::models::AppState;
use crate::models::Problem;
use axum::Json;
use axum::extract::Query;
use axum::{Router, extract::State, response::IntoResponse, routing::get};
use std::sync::Arc;

pub fn routes(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/instances", get(get_all))
        .with_state(state)
}

/// A page of the instances, newest id first.
#[utoipa::path(
    get,
    path = "/api/v1/instances",
    operation_id = "list_instances",
    tag = "instances",
    params(Paging),
    responses((status = 200, description = "A page of instances", body = PagingResult<InstanceRow>))
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
