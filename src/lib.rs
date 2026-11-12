use std::sync::Arc;

use axum::Router;
use tower_http::trace::TraceLayer;

pub mod db;
pub mod features;
pub mod handlers;
pub mod models;
pub mod otel;
pub mod services;

pub use models::AppState;

/// HTTP API, UI and probes with the tracing layer.
pub fn app(state: &Arc<AppState>) -> Router {
    // the API: behind the token when --api-token is set (route_layer: an unknown path
    // stays a 404)
    let api = Router::new()
        .merge(handlers::http::routes(Arc::clone(state)))
        .nest("/api/v1", handlers::jobs::routes(Arc::clone(state)))
        .nest("/api/v1", handlers::openapi::routes())
        .nest("/api/v1", features::history::routes(Arc::clone(state)))
        .nest("/api/v1", features::results::routes(Arc::clone(state)))
        .nest("/api/v1", features::schedules::routes(Arc::clone(state)))
        .nest("/api/v1", features::flows::routes(Arc::clone(state)))
        .nest("/api/v1", features::instances::routes(Arc::clone(state)))
        .route_layer(axum::middleware::from_fn_with_state(
            Arc::clone(state),
            handlers::auth::require_api_token,
        ));
    let router = Router::new()
        .merge(api)
        .merge(features::live::routes(Arc::clone(state)))
        .merge(features::ui::routes(Arc::clone(state)));
    // /echo and /error answer whatever is asked (a status, a delay): for development and
    // the tests only
    let router = if state.environment.is_development() {
        router.merge(features::echo::routes(Arc::clone(state)))
    } else {
        router
    };
    router
        .layer(axum::middleware::from_fn(otel::metrics::track_http_server))
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(otel::make_span_from_request)
                .on_response(otel::on_response)
                .on_failure(otel::on_failure),
        )
}
