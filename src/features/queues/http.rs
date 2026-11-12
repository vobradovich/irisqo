use super::{QueueRequest, QueueRow, UndeclaredQueueRow};
use crate::features::{Paging, PagingResult};
use crate::handlers::http::parse_body;
use crate::models::{AppState, Error, Problem};
use axum::{
    Json, Router,
    body::Bytes,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use std::sync::Arc;

pub fn routes(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/queues", get(get_all))
        .route("/queues/{id}", get(get_by_id).put(upsert).delete(delete))
        .route("/queues/{id}/pause", post(pause))
        .route("/queues/{id}/resume", post(resume))
        .route("/undeclared-queues", get(get_undeclared))
        .with_state(state)
}

/// A page of the declared queues by name.
#[utoipa::path(
    get,
    path = "/api/v1/queues",
    operation_id = "list_queues",
    tag = "queues",
    params(Paging),
    responses((status = 200, description = "A page of queues", body = PagingResult<QueueRow>))
)]
pub(crate) async fn get_all(
    State(state): State<Arc<AppState>>,
    Query(pagination): Query<Paging>,
) -> Result<impl IntoResponse, Problem> {
    let (limit, offset) = pagination.or_default();
    let data = super::db::list(&state.pool, None, limit.into(), offset.into()).await?;
    Ok(Json(PagingResult {
        limit,
        offset,
        data,
    }))
}

/// A page, by name, of the names that jobs use without a declaration, with their jobs counted.
#[utoipa::path(
    get,
    path = "/api/v1/undeclared-queues",
    operation_id = "list_undeclared_queues",
    tag = "queues",
    params(Paging),
    responses((status = 200, description = "A page of names", body = PagingResult<UndeclaredQueueRow>))
)]
pub(crate) async fn get_undeclared(
    State(state): State<Arc<AppState>>,
    Query(pagination): Query<Paging>,
) -> Result<impl IntoResponse, Problem> {
    let (limit, offset) = pagination.or_default();
    let data = super::db::list_undeclared(&state.pool, None, limit.into(), offset.into()).await?;
    Ok(Json(PagingResult {
        limit,
        offset,
        data,
    }))
}

/// A declared queue.
#[utoipa::path(
    get,
    path = "/api/v1/queues/{id}",
    operation_id = "get_queue",
    tag = "queues",
    params(("id" = String, Path, description = "The name of the queue")),
    responses(
        (status = 200, description = "The queue", body = QueueRow),
        (status = 404, description = "No such queue", body = Problem, content_type = "application/problem+json"),
    )
)]
pub(crate) async fn get_by_id(
    State(state): State<Arc<AppState>>,
    Path(queue): Path<String>,
) -> Result<Response, Problem> {
    let row = found(super::db::get_by_id(&state.pool, &queue).await?, &queue)?;
    Ok(Json(row).into_response())
}

/// Declares a queue or replaces the limits of a declared one: what the body leaves out takes its
/// default, a pause stays. `201` when the queue is new, `200` when it is changed; no body is all
/// the defaults.
#[utoipa::path(
    put,
    path = "/api/v1/queues/{id}",
    operation_id = "put_queue",
    tag = "queues",
    params(("id" = String, Path, description = "The name of the queue")),
    request_body = QueueRequest,
    responses(
        (status = 200, description = "The queue is changed", body = QueueRow),
        (status = 201, description = "The queue is new", body = QueueRow),
        (status = 400, description = "Invalid fields (RFC 9457 problem with `errors`)", body = Problem, content_type = "application/problem+json"),
    )
)]
pub(crate) async fn upsert(
    State(state): State<Arc<AppState>>,
    Path(queue): Path<String>,
    body: Bytes,
) -> Result<Response, Problem> {
    let create = parse_request(&body)?
        .validate(&queue)
        .map_err(Error::InvalidParams)?;
    let (row, created) = super::db::upsert(&state.pool, &create).await?;
    let status = if created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((status, Json(row)).into_response())
}

/// Pauses the queue: its jobs wait, none is lost; the ones running finish.
#[utoipa::path(
    post,
    path = "/api/v1/queues/{id}/pause",
    operation_id = "pause_queue",
    tag = "queues",
    params(("id" = String, Path, description = "The name of the queue")),
    responses(
        (status = 204, description = "Paused"),
        (status = 404, description = "No such queue"),
    )
)]
pub(crate) async fn pause(
    State(state): State<Arc<AppState>>,
    Path(queue): Path<String>,
) -> Result<Response, Problem> {
    let done = super::db::pause(&state.pool, &queue).await?;
    Ok(found_status(done).into_response())
}

/// Resumes the queue.
#[utoipa::path(
    post,
    path = "/api/v1/queues/{id}/resume",
    operation_id = "resume_queue",
    tag = "queues",
    params(("id" = String, Path, description = "The name of the queue")),
    responses(
        (status = 204, description = "Resumed"),
        (status = 404, description = "No such queue"),
    )
)]
pub(crate) async fn resume(
    State(state): State<Arc<AppState>>,
    Path(queue): Path<String>,
) -> Result<Response, Problem> {
    let done = super::db::resume(&state.pool, &queue).await?;
    Ok(found_status(done).into_response())
}

/// Removes the declaration. The jobs stay; the name goes back to one job at a time, in order.
#[utoipa::path(
    delete,
    path = "/api/v1/queues/{id}",
    operation_id = "delete_queue",
    tag = "queues",
    params(("id" = String, Path, description = "The name of the queue")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 404, description = "No such queue"),
    )
)]
pub(crate) async fn delete(
    State(state): State<Arc<AppState>>,
    Path(queue): Path<String>,
) -> Result<Response, Problem> {
    let done = super::db::delete(&state.pool, &queue).await?;
    Ok(found_status(done).into_response())
}

/// The limits of the body; no body is all the defaults.
fn parse_request(body: &[u8]) -> Result<QueueRequest, Error> {
    if body.is_empty() {
        Ok(QueueRequest::default())
    } else {
        parse_body(body, "queue")
    }
}

fn found(row: Option<QueueRow>, queue: &str) -> Result<QueueRow, Error> {
    row.ok_or_else(|| Error::QueueNotFound(queue.to_owned()))
}

fn found_status(found: bool) -> StatusCode {
    if found {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::NOT_FOUND
    }
}
