use crate::models::AppState;
use crate::models::Error;
use crate::models::Problem;
use axum::{
    Router,
    extract::State,
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Redirect, Response},
    routing::get,
};
use sqlx::{Pool, Postgres};
use std::sync::Arc;

pub fn routes(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(root))
        .route("/live", get(live))
        .route("/ready", get(ready))
        .with_state(state)
}

/// A browser goes to the UI; curl and the like get a hint on the API.
async fn root(headers: HeaderMap) -> Response {
    let browser = headers
        .get(header::ACCEPT)
        .and_then(|accept| accept.to_str().ok())
        .is_some_and(|accept| accept.contains("text/html"));
    if browser {
        Redirect::to("/ui/jobs").into_response()
    } else {
        (
            StatusCode::OK,
            "Use /to/* to queue a request, /ui for the UI.",
        )
            .into_response()
    }
}

async fn live() -> impl IntoResponse {
    StatusCode::OK
}

async fn ready(State(state): State<Arc<AppState>>) -> Result<StatusCode, Problem> {
    select_one(&state.pool).await?;
    Ok(StatusCode::OK)
}

async fn select_one(pool: &Pool<Postgres>) -> Result<(), Error> {
    _ = sqlx::query("SELECT 1").fetch_one(pool).await?;
    Ok(())
}
