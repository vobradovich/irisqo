use std::net::SocketAddr;
use std::sync::Arc;

use irisqo::{AppState, db, services};
use opentelemetry::trace::TracerProvider as _;
use tokio::net::TcpListener;
use tokio::signal;
use tokio_util::sync::CancellationToken;
use tracing::Level;
use tracing_subscriber::{
    EnvFilter, Layer, filter::Targets, layer::SubscriberExt, util::SubscriberInitExt,
};

#[tokio::main]
async fn main() {
    let provider = irisqo::otel::init_tracer_provider();
    let tracer = provider.tracer("irisqo");

    // RUST_LOG filters the logs only: the exported spans are the `otel::tracing` ones
    let log_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "irisqo=debug,tower_http=debug,sqlx=info".into());
    let span_filter = Targets::new().with_target(irisqo::otel::TRACING_TARGET, Level::INFO);
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().with_filter(log_filter))
        .with(
            tracing_opentelemetry::layer()
                .with_tracer(tracer)
                .with_filter(span_filter),
        )
        .init();

    let state = AppState::new().await;
    // The heartbeat outlives the other services: it stops only once the workers are done.
    let heartbeat_stop = CancellationToken::new();
    let heartbeat = tokio::spawn({
        let state = Arc::clone(&state);
        let stop = heartbeat_stop.clone();
        async move { services::heartbeat::run(&state, stop).await }
    });
    tokio::join!(
        start_http_server(&state),
        services::start_scheduler_service(&state),
        services::start_retention_service(&state),
        services::start_worker_service(&state),
    );
    heartbeat_stop.cancel();
    _ = heartbeat
        .await
        .inspect_err(|err| tracing::error!("heartbeat error {:?}", err));

    // All workers have finished their jobs: release whatever is still locked by this instance.
    if let Err(err) = db::instances::kill(&state.pool, &state.instance_id).await {
        tracing::error!("db::instances::kill error {:?}", err);
    }
    // Flush the spans still in the batch
    if let Err(err) = provider.shutdown() {
        eprintln!("tracer provider shutdown error {err:?}");
    }
    eprintln!("->> SHUTDOWN")
}

async fn start_http_server(state: &Arc<AppState>) {
    let addr: SocketAddr = SocketAddr::from(([0, 0, 0, 0], state.port));
    let app = irisqo::app(state);

    let listener = TcpListener::bind(addr).await.expect("Failed to bind");
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
