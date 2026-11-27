use std::net::SocketAddr;
use std::sync::Arc;

use irisqo::{
    AppState,
    cli::Command,
    db::{self, setup},
    models::{DbOptions, Options},
    otel::{self, Telemetry},
    services,
};
use tokio::net::TcpListener;
use tokio::signal;
use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() {
    match Command::load() {
        Command::Serve(options) => serve(options).await,
        // only the SQL on stdout: no telemetry, which may log there
        Command::MigrationsSql {
            schema,
            create_schema,
        } => print!("{}", setup::migrations_sql(&schema, create_schema)),
        Command::MigrationsRun { options, recreate } => {
            let telemetry = Telemetry::init(options.environment);
            migrate(&options.db, recreate).await;
            telemetry.shutdown();
        }
    }
}

/// `irisqo migrations run`: creates the schema (dropping it first with `--recreate`) and runs the migrations.
async fn migrate(db: &DbOptions, recreate: bool) {
    let schema = db.schema.as_str();
    let pool = setup::connect(db, "irisqo migrations run").await;
    if recreate {
        tracing::warn!(schema, "--recreate: dropping the schema with all its data");
        setup::drop_schema(&pool, schema).await;
    }
    setup::migrate_schema(&pool, db).await;
    pool.close().await;
    tracing::info!(schema, "migrations applied");
}

async fn serve(options: Options) {
    let telemetry = Telemetry::init(options.environment);

    let state = AppState::new(options).await;
    let roles = state.options.effective_roles();
    tracing::info!(environment = %state.options.environment, %roles, "starting");
    if roles.serves_clients()
        && state.options.api_tokens.is_none()
        && !state.options.environment.is_development()
    {
        tracing::warn!(
            "no --api-token (IRISQO_API_TOKEN): the API is open to anyone who reaches it"
        );
    }
    otel::metrics::register_pool_metrics(&state.pool);
    // The heartbeat outlives the other services: it stops only once the workers are done.
    let heartbeat_stop = CancellationToken::new();
    let heartbeat = tokio::spawn({
        let state = Arc::clone(&state);
        let stop = heartbeat_stop.clone();
        async move { services::heartbeat::run(&state, stop).await }
    });
    tokio::join!(
        listen(&state),
        stop_on_signal(&state),
        services::run(&state),
    );
    heartbeat_stop.cancel();
    _ = heartbeat
        .await
        .inspect_err(|err| tracing::error!("heartbeat error {:?}", err));

    // All workers have finished their jobs: release whatever is still locked by this instance.
    if let Err(err) = db::instances::kill(&state.pool, &state.instance_id).await {
        tracing::error!("db::instances::kill error {:?}", err);
    }
    // Flush the spans, metrics and logs still in the batches
    telemetry.shutdown();
    eprintln!("->> SHUTDOWN")
}

/// Serves what the roles of the instance serve (the probes always) until shutdown.
async fn listen(state: &Arc<AppState>) {
    let addr: SocketAddr = SocketAddr::from(([0, 0, 0, 0], state.options.port));
    let app = irisqo::app(state);

    let listener = TcpListener::bind(addr).await.expect("Failed to bind");
    tracing::info!("listen {:?}", addr);
    axum::serve(listener, app.into_make_service())
        .with_graceful_shutdown(state.shutdown_token.clone().cancelled_owned())
        .await
        .expect("Failed to run axum::serve");
}

/// The signal, not the end of the server, starts the shutdown: an instance without the
/// API and the UI has nothing else to tell it.
async fn stop_on_signal(state: &AppState) {
    shutdown_signal().await;
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
