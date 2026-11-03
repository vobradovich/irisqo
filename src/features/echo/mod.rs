use crate::models::AppState;
use axum::{
    Json, Router,
    body::Body,
    extract::Query,
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::IntoResponse,
    routing::get,
};
use hyper::Method;

use std::{collections::HashMap, sync::Arc, time::Duration};
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

/// Echoes the body and `content*` headers back. For tests:
/// - `?status=429&retry-after=120` set the response status and `Retry-After`;
/// - `?headers=1` answers with the request headers as a JSON object instead of the body;
/// - `?sleep_ms=500` answers after a delay.
async fn echo(
    method: Method,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: Body,
) -> impl IntoResponse {
    if let Some(ms) = params.get("sleep_ms").and_then(|ms| ms.parse().ok()) {
        tokio::time::sleep(Duration::from_millis(ms)).await;
    }
    let mut response = if params.contains_key("headers") {
        let received: HashMap<&str, &str> = headers
            .iter()
            .filter_map(|(k, v)| Some((k.as_str(), v.to_str().ok()?)))
            .collect();
        Json(received).into_response()
    } else {
        body.into_response()
    };
    if let Some(status) = params
        .get("status")
        .and_then(|s| s.parse::<StatusCode>().ok())
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
    if !params.contains_key("headers") {
        for (k, v) in headers {
            if let Some(key) = k
                && key.as_str().starts_with("content")
            {
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
