//! The API token: `Authorization: Bearer <token>` on `/to/*` and `/api/v1/*` when
//! `--api-token` is set. The caller's own `Authorization` never reaches the target: only
//! the `Irisqo-Forward-*` headers are forwarded.

use std::sync::Arc;

use axum::{
    extract::{Request, State},
    http::{HeaderValue, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};

use crate::models::{AppState, Problem};

pub async fn require_api_token(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Response {
    if state.api_tokens.is_empty() {
        return next.run(request).await;
    }
    let token = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(bearer);
    match token {
        Some(token) if state.api_tokens.accepts(token) => next.run(request).await,
        // RFC 6750: no credentials, or wrong ones
        Some(_) => unauthorized("Bearer realm=\"irisqo\", error=\"invalid_token\""),
        None => unauthorized("Bearer realm=\"irisqo\""),
    }
}

/// The token of `Bearer <token>` (the scheme in any case).
fn bearer(value: &str) -> Option<&str> {
    let (scheme, token) = value.split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then(|| token.trim())
}

fn unauthorized(challenge: &'static str) -> Response {
    let problem = Problem {
        detail: Some("Send a valid API token: Authorization: Bearer <token>.".to_owned()),
        ..Problem::new(StatusCode::UNAUTHORIZED)
    };
    let mut response = problem.into_response();
    response.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        HeaderValue::from_static(challenge),
    );
    response
}

#[test]
fn bearer_tokens() {
    assert_eq!(bearer("Bearer abc"), Some("abc"));
    assert_eq!(bearer("bearer  abc "), Some("abc"));
    assert_eq!(bearer("Basic abc"), None);
    assert_eq!(bearer("Bearer"), None);
}
