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
    let router = Router::new()
        .merge(handlers::http::routes(Arc::clone(state)))
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
        .nest("/api/v1", handlers::jobs::routes(Arc::clone(state)))
        .nest("/api/v1", features::history::routes(Arc::clone(state)))
        .nest("/api/v1", features::results::routes(Arc::clone(state)))
        .nest("/api/v1", features::schedules::routes(Arc::clone(state)))
        .nest("/api/v1", features::instances::routes(Arc::clone(state)))
        .layer(axum::middleware::from_fn(otel::metrics::track_http_server))
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(otel::make_span_from_request)
                .on_response(otel::on_response)
                .on_failure(otel::on_failure),
        )
}
