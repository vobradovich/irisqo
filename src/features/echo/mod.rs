use crate::models::AppState;
use axum::{
    body::Body,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::get,
    Router,
};
use hyper::Method;

use std::sync::Arc;
#[allow(unused_imports)]
use tracing::{debug, error, info, warn};

pub fn routes(state: Arc<AppState>) -> Router {
    Router::new()
        .route(
            "/echo",
            get(echo).post(echo).put(echo).patch(echo).delete(echo),
        )
        .route(
            "/error",
            get(error).post(error).put(error).patch(error).delete(error),
        )
        .with_state(state)
}

async fn echo(method: Method, headers: HeaderMap, body: Body) -> impl IntoResponse {
    let mut response = body.into_response();
    let response_headers = response.headers_mut();
    response_headers.insert("http-method", method.as_str().parse().unwrap());
    for (k, v) in headers {
        if let Some(key) = k {
            if key.as_str().starts_with("content") {
                response_headers.insert(key, v);
            }
        }
    }
    response
}

async fn error() -> impl IntoResponse {
    let trace_id = crate::otel::current_trace_id();
    problemdetails::new(StatusCode::INTERNAL_SERVER_ERROR)
        // .with_type("https://example.com/probs/out-of-credit")
        .with_title(StatusCode::INTERNAL_SERVER_ERROR.to_string())
        .with_value("trace_id", trace_id)
}
