use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use models::AppState;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_sdk::trace::TracerProvider;
use opentelemetry_stdout as stdout;
use tokio::net::TcpListener;
use tokio::signal;
use tower_http::trace::TraceLayer;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

mod db;
mod features;
mod handlers;
mod models;
mod otel;
mod services;

#[tokio::main]
async fn main() {
    opentelemetry::global::set_text_map_propagator(
        opentelemetry_sdk::propagation::TraceContextPropagator::new(),
    );
    // Create a new OpenTelemetry trace pipeline that prints to stdout
    let provider = TracerProvider::builder()
        .with_simple_exporter(stdout::SpanExporter::default())
        .build();

    let tracer = provider.tracer("irisqo");
    opentelemetry::global::set_tracer_provider(provider);

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "irisqo=debug,tower_http=debug,otel=info,sqlx=info".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .with(tracing_opentelemetry::layer().with_tracer(tracer))
        .init();

    let state = AppState::new().await;
    tokio::join!(
        start_http_server(&state),
        services::start_scheduler_service(&state),
        services::start_channel_jobs_service(&state),
        services::start_batch_jobs_service(&state),
    );

    eprintln!("->> SHUTDOWN")
}

async fn start_http_server(state: &Arc<AppState>) {
    let addr: SocketAddr = SocketAddr::from(([0, 0, 0, 0], state.port));
    let app = Router::new()
        .merge(handlers::http::routes(Arc::clone(state)))
        .merge(features::live::routes(Arc::clone(state)))
        .merge(features::echo::routes(Arc::clone(state)))
        .nest("/api/v1", handlers::jobs::routes(Arc::clone(state)))
        .nest("/api/v1", features::history::routes(Arc::clone(state)))
        .nest("/api/v1", features::results::routes(Arc::clone(state)))
        .nest("/api/v1", features::schedules::routes(Arc::clone(state)))
        .nest("/api/v1", features::instances::routes(Arc::clone(state)))
        .layer(TraceLayer::new_for_http().make_span_with(otel::make_span_from_request));

    let listener = TcpListener::bind(addr).await.unwrap();
    tracing::info!("listen {:?}", addr);
    axum::serve(listener, app.into_make_service())
        .with_graceful_shutdown(shutdown_signal())
        .await
        .expect("Failed to run axum::serve");
    state.shutdown_token.cancel();
}

async fn shutdown_signal() {
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
    tracing::warn!("signal received, starting graceful shutdown");
}
