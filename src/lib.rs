use std::sync::Arc;

use axum::Router;
use tower_http::trace::TraceLayer;

pub mod cli;
pub mod db;
pub mod features;
pub mod handlers;
pub mod models;
pub mod otel;
pub mod services;

pub use models::AppState;
use models::Role;

/// What the roles of the instance serve, with the tracing layer: the probes always, the API
/// for `Api`, the pages for `Ui`.
pub fn app(state: &Arc<AppState>) -> Router {
    let roles = &state.options.roles;
    let mut router = features::live::probes(Arc::clone(state));
    if roles.serves_clients() {
        router = router.merge(features::live::root_route(Arc::clone(state)));
    }
    // every role is spelled out: a new one cannot be forgotten here
    for role in roles.iter() {
        match role {
            Role::Api => router = router.merge(api_routes(state)),
            Role::Ui => router = router.merge(features::ui::routes(Arc::clone(state))),
            // they run in `services::run`, there is nothing to serve
            Role::Worker | Role::Scheduler | Role::Retention => {}
        }
    }
    router
        .layer(axum::middleware::from_fn(otel::metrics::track_http_server))
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(otel::make_span_from_request)
                .on_response(otel::on_response)
                .on_failure(otel::on_failure),
        )
}

/// `/to/*` and `/api/v1/*` behind the token when --api-token is set (route_layer: an unknown
/// path stays a 404); in development also `/echo` and `/error`, which answer whatever is asked
/// (a status, a delay): for development and the tests only.
fn api_routes(state: &Arc<AppState>) -> Router {
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
    if state.options.environment.is_development() {
        api.merge(features::echo::routes(Arc::clone(state)))
    } else {
        api
    }
}
