use super::{FlowRequest, FlowRow, UndeclaredFlowRow};
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
        .route("/flows", get(get_all))
        .route("/flows/{id}", get(get_by_id).put(upsert).delete(delete))
        .route("/flows/{id}/pause", post(pause))
        .route("/flows/{id}/resume", post(resume))
        .route("/undeclared-flows", get(get_undeclared))
        .with_state(state)
}

/// A page of the declared flows by name.
#[utoipa::path(
    get,
    path = "/api/v1/flows",
    operation_id = "list_flows",
    tag = "flows",
    params(Paging),
    responses((status = 200, description = "A page of flows", body = PagingResult<FlowRow>))
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
    path = "/api/v1/undeclared-flows",
    operation_id = "list_undeclared_flows",
    tag = "flows",
    params(Paging),
    responses((status = 200, description = "A page of names", body = PagingResult<UndeclaredFlowRow>))
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

/// A declared flow.
#[utoipa::path(
    get,
    path = "/api/v1/flows/{id}",
    operation_id = "get_flow",
    tag = "flows",
    params(("id" = String, Path, description = "The name of the flow")),
    responses(
        (status = 200, description = "The flow", body = FlowRow),
        (status = 404, description = "No such flow", body = Problem, content_type = "application/problem+json"),
    )
)]
pub(crate) async fn get_by_id(
    State(state): State<Arc<AppState>>,
    Path(flow): Path<String>,
) -> Result<Response, Problem> {
    let row = found(super::db::get_by_id(&state.pool, &flow).await?, &flow)?;
    Ok(Json(row).into_response())
}

/// Declares a flow or replaces the limits of a declared one: what the body leaves out takes its
/// default, a pause stays. `201` when the flow is new, `200` when it is changed; no body is all
/// the defaults.
#[utoipa::path(
    put,
    path = "/api/v1/flows/{id}",
    operation_id = "put_flow",
    tag = "flows",
    params(("id" = String, Path, description = "The name of the flow")),
    request_body = FlowRequest,
    responses(
        (status = 200, description = "The flow is changed", body = FlowRow),
        (status = 201, description = "The flow is new", body = FlowRow),
        (status = 400, description = "Invalid fields (RFC 9457 problem with `errors`)", body = Problem, content_type = "application/problem+json"),
    )
)]
pub(crate) async fn upsert(
    State(state): State<Arc<AppState>>,
    Path(flow): Path<String>,
    body: Bytes,
) -> Result<Response, Problem> {
    let create = parse_request(&body)?
        .validate(&flow)
        .map_err(Error::InvalidParams)?;
    let (row, created) = super::db::upsert(&state.pool, &create).await?;
    let status = if created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((status, Json(row)).into_response())
}

/// Pauses the flow: its jobs wait, none is lost; the ones running finish.
#[utoipa::path(
    post,
    path = "/api/v1/flows/{id}/pause",
    operation_id = "pause_flow",
    tag = "flows",
    params(("id" = String, Path, description = "The name of the flow")),
    responses(
        (status = 204, description = "Paused"),
        (status = 404, description = "No such flow"),
    )
)]
pub(crate) async fn pause(
    State(state): State<Arc<AppState>>,
    Path(flow_id): Path<String>,
) -> Result<Response, Problem> {
    let done = super::db::pause(&state.pool, &flow_id).await?;
    Ok(found_status(done).into_response())
}

/// Resumes the flow.
#[utoipa::path(
    post,
    path = "/api/v1/flows/{id}/resume",
    operation_id = "resume_flow",
    tag = "flows",
    params(("id" = String, Path, description = "The name of the flow")),
    responses(
        (status = 204, description = "Resumed"),
        (status = 404, description = "No such flow"),
    )
)]
pub(crate) async fn resume(
    State(state): State<Arc<AppState>>,
    Path(flow_id): Path<String>,
) -> Result<Response, Problem> {
    let done = super::db::resume(&state.pool, &flow_id).await?;
    Ok(found_status(done).into_response())
}

/// Removes the declaration. The jobs stay; the name goes back to one job at a time.
#[utoipa::path(
    delete,
    path = "/api/v1/flows/{id}",
    operation_id = "delete_flow",
    tag = "flows",
    params(("id" = String, Path, description = "The name of the flow")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 404, description = "No such flow"),
    )
)]
pub(crate) async fn delete(
    State(state): State<Arc<AppState>>,
    Path(flow_id): Path<String>,
) -> Result<Response, Problem> {
    let done = super::db::delete(&state.pool, &flow_id).await?;
    Ok(found_status(done).into_response())
}

/// The limits of the body; no body is all the defaults.
fn parse_request(body: &[u8]) -> Result<FlowRequest, Error> {
    if body.is_empty() {
        Ok(FlowRequest::default())
    } else {
        parse_body(body, "flow")
    }
}

fn found(row: Option<FlowRow>, flow_id: &str) -> Result<FlowRow, Error> {
    row.ok_or_else(|| Error::FlowNotFound(flow_id.to_owned()))
}

fn found_status(found: bool) -> StatusCode {
    if found {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::NOT_FOUND
    }
}
