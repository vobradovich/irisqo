use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use models::AppState;
use tokio::signal;
use tower_http::trace::TraceLayer;
use tracing_subscriber::prelude::__tracing_subscriber_SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

mod db;
mod handlers;
mod models;
mod services;

#[tokio::main]
async fn main() {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                "irisqo=debug,tower_http=debug,axum_tracing_opentelemetry=debug,otel=debug".into()
            }),
        )
        .with(tracing_opentelemetry::layer())
        .with(tracing_subscriber::fmt::layer())
        .init();

    let state = AppState::new().await;
    tokio::join!(
        start_http_server(&state),
        start_scheduler_service(&state),
        start_jobs_service(&state)
    );

    println!("->> SHUTDOWN")
}

async fn start_http_server(state: &Arc<AppState>) {
    let addr = SocketAddr::from(([0, 0, 0, 0], 8102));
    let app = Router::new()
        .merge(handlers::http::routes(Arc::clone(state)))
        .merge(handlers::live::routes(Arc::clone(state)))
        .nest("/api/v1", handlers::jobs::routes(Arc::clone(state)))
        .layer(TraceLayer::new_for_http());

    axum::Server::bind(&addr)
        .serve(app.into_make_service())
        .with_graceful_shutdown(shutdown_signal(Arc::clone(state)))
        .await
        .unwrap();
}

async fn start_scheduler_service(state: &Arc<AppState>) {
    let app_state = Arc::clone(state);
    let service = services::SchedulerService::new(app_state);
    service.run().await.expect("Failed to run SchedulerService");
}

async fn start_jobs_service(state: &Arc<AppState>) {
    let app_state = Arc::clone(state);
    let service = services::NaiveWorkerService::new(app_state);
    service.run().await.expect("Failed to run JobService");
}

async fn shutdown_signal(state: Arc<AppState>) {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    state.shutdown_token.cancel();
    tracing::warn!("signal received, starting graceful shutdown");
}
