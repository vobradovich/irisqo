use crate::models::AppState;
use axum::{
    Router,
    body::Body,
    extract::Query,
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::IntoResponse,
    routing::get,
};
use hyper::Method;

use std::{collections::HashMap, sync::Arc};
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

/// Echoes the body and `content*` headers back.
/// `?status=429&retry-after=120` set the response status and `Retry-After` (for tests).
async fn echo(
    method: Method,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: Body,
) -> impl IntoResponse {
    let mut response = body.into_response();
    if let Some(status) = params
        .get("status")
        .and_then(|s| s.parse::<u16>().ok())
        .and_then(|s| StatusCode::from_u16(s).ok())
    {
        *response.status_mut() = status;
    }
    let response_headers = response.headers_mut();
    if let Some(value) = params
        .get("retry-after")
        .and_then(|v| HeaderValue::from_str(v).ok())
    {
        response_headers.insert(header::RETRY_AFTER, value);
    }
    if let Ok(value) = HeaderValue::from_str(method.as_str()) {
        response_headers.insert("http-method", value);
    }
    for (k, v) in headers {
        if let Some(key) = k
            && key.as_str().starts_with("content")
        {
            response_headers.insert(key, v);
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
