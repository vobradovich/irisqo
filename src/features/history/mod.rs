use crate::models::{AppState, Error};
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    response::IntoResponse,
    routing::get,
};
use problemdetails::Problem;
use sqlx::{Pool, Postgres};
use std::sync::Arc;

pub use jobhistory::JobHistoryRow;

use super::{Paging, PagingResult};

mod jobhistory;

pub fn routes(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/jobs/{id}/history", get(get_history_by_id))
        .with_state(state)
}

async fn get_history_by_id(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
    Query(pagination): Query<Paging>,
) -> Result<impl IntoResponse, Problem> {
    let limit = pagination.limit.unwrap_or(100);
    let offset = pagination.offset.unwrap_or(0);
    let data = get_by_id(&state.pool, id, limit, offset).await?;
    Ok(Json(PagingResult {
        limit,
        offset,
        data,
    }))
}

pub async fn get_by_id(
    pool: &Pool<Postgres>,
    job_id: i64,
    limit: i32,
    offset: i32,
) -> Result<Vec<JobHistoryRow>, Error> {
    const SQL: &str = "SELECT id, retry, instance_id, status::text, at, message FROM history WHERE id = $1 ORDER BY at LIMIT $2 OFFSET $3";
    let history = sqlx::query_as::<_, JobHistoryRow>(SQL)
        .bind(job_id)
        .bind(limit)
        .bind(offset)
        .fetch_all(pool)
        .await?;
    Ok(history)
}
