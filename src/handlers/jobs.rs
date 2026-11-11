use super::{
    http::{create_job, job_headers, parse_body},
    job_input::{JobInput, prepare_input},
};
use crate::models::{Error, Problem};
use crate::{db, db::jobqueue::ActionOutcome, models::AppState};
use axum::{
    Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use chrono::Utc;
use std::sync::Arc;

pub fn routes(state: Arc<AppState>) -> Router {
    Router::new()
        .route(
            "/jobs",
            post(create).layer(DefaultBodyLimit::max(state.max_body_size)),
        )
        .route("/jobs/{id}", get(get_by_id).delete(delete_by_id))
        .route("/jobs/{id}/cancel", post(cancel))
        .route("/jobs/{id}/retry", post(retry))
        .with_state(state)
}

/// Creates a job from JSON: `200` with the job (`id`, `key`) and the same in the headers
/// (`Location`, `Irisqo-*`). A repeated `key` returns the existing job.
async fn create(
    State(state): State<Arc<AppState>>,
    body: Bytes,
) -> Result<impl IntoResponse, Problem> {
    let spec: JobInput = parse_body(&body, "job")?;
    let job =
        prepare_input(&state.worker_options, spec, Utc::now()).map_err(Error::InvalidParams)?;
    let job = create_job(&state, job).await?;
    Ok((job_headers(&job), Json(job)))
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
    let job = db::jobqueue::get_by_id(&state.pool, id)
        .await?
        .ok_or(Error::JobNotFound(id))?;
    Ok(Json(job).into_response())
}

fn action_status(outcome: ActionOutcome) -> StatusCode {
    match outcome {
        ActionOutcome::Done => StatusCode::NO_CONTENT,
        ActionOutcome::Conflict => StatusCode::CONFLICT,
        ActionOutcome::NotFound => StatusCode::NOT_FOUND,
    }
}

/// Cancels a pending job; 409 when it is running or already processed.
async fn cancel(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> Result<StatusCode, Problem> {
    let outcome = db::jobqueue::cancel_job(&state.pool, id, &state.instance_id).await?;
    Ok(action_status(outcome))
}

/// Runs a processed job once more; 409 when it is still pending or running.
async fn retry(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> Result<StatusCode, Problem> {
    let outcome = db::jobqueue::requeue_job(&state.pool, id, &state.instance_id).await?;
    Ok(action_status(outcome))
}
