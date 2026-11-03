use bytes::Bytes;
use dotenvy::dotenv;
use http_body_util::Full;
use hyper_tls::HttpsConnector;
use hyper_util::client::legacy::{Client, connect::HttpConnector};
use hyper_util::rt::TokioExecutor;
use sqlx::{
    Pool, Postgres,
    postgres::{PgConnectOptions, PgPoolOptions},
};
use std::{str::FromStr, sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;

//type DbPool = Pool<Postgres>;
#[derive(Debug)]
pub struct AppState {
    pub port: u16,
    pub instance_id: String,
    pub pool: Pool<Postgres>,
    pub client: Client<HttpsConnector<HttpConnector>, Full<Bytes>>,
    pub scheduler_options: Option<SchedulerOptions>,
    pub worker_options: WorkerOptions,
    pub shutdown_token: CancellationToken,
}

#[derive(Debug)]
pub struct SchedulerOptions {
    pub poll_interval: Duration,
    pub prefetch: u16,
}

#[derive(Debug)]
pub struct WorkerOptions {
    pub workers_count: usize,
    /// Total number of jobs in flight for `ConcurrentWorkerService`; `None` selects `ChannelWorkerService`.
    pub concurrency: Option<usize>,
    pub poll_interval: Duration,
    pub prefetch: u16,
    pub timeout: u32,
}

/// Command line options.
#[derive(Debug, Clone, Default)]
pub struct Options {
    pub port: Option<u16>,
    pub workers: Option<usize>,
    pub db_connections: Option<u32>,
    pub concurrency: Option<usize>,
    pub interval: Option<u64>,
    pub prefetch: Option<u16>,
    pub timeout: Option<u32>,
}

impl Options {
    #[must_use]
    pub fn from_args() -> Options {
        let flags = xflags::parse_or_exit! {
            /// API Port number. Default: 8102
            optional -p,--port n:u16
            /// Workers count. Default: 8
            optional -w,--workers n:usize
            /// Max db connections (keep below the Postgres max_connections). Default: 32
            optional --db-connections n:u32
            /// Jobs in flight across all cores (enables the concurrent worker). Default: off
            optional --concurrency n:usize
            /// Worker Poll Interval in milliseconds. Default: 1000
            optional --interval n:u64
            /// Prefetch count. Default: 8
            optional --prefetch n:u16
            /// Default job timeout in milliseconds. Default: 3000
            optional --timeout n:u32
        };
        Options {
            port: flags.port,
            workers: flags.workers,
            db_connections: flags.db_connections,
            concurrency: flags.concurrency,
            interval: flags.interval,
            prefetch: flags.prefetch,
            timeout: flags.timeout,
        }
    }
}

impl AppState {
    /// Parses the command line, connects to `DATABASE_URL` and runs the migrations.
    #[must_use]
    pub async fn new() -> Arc<AppState> {
        let options = Options::from_args();

        dotenv().ok();
        let instance_id = new_instance_id();
        let db_url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");
        let conn = PgConnectOptions::from_str(&db_url)
            .expect("Unable to parse DATABASE_URL")
            .application_name(&instance_id);

        let pool = PgPoolOptions::new()
            .max_connections(options.db_connections.unwrap_or(32))
            .connect_with(conn)
            .await
            .expect("Unable to connect to Postgres");

        sqlx::migrate!("./sql/migrations")
            .run(&pool)
            .await
            .expect("Unable to run migrations");

        Self::with_pool(pool, instance_id, &options)
    }

    /// Builds the state on top of an already connected (and migrated) pool.
    #[must_use]
    pub fn with_pool(
        pool: Pool<Postgres>,
        instance_id: String,
        options: &Options,
    ) -> Arc<AppState> {
        let https = hyper_tls::HttpsConnector::new();
        let state = AppState {
            port: options.port.unwrap_or(8102),
            instance_id,
            pool,
            client: Client::builder(TokioExecutor::new()).build::<_, Full<Bytes>>(https),
            scheduler_options: Some(SchedulerOptions {
                poll_interval: Duration::from_millis(5000),
                prefetch: 1000,
            }),
            worker_options: WorkerOptions {
                workers_count: options.workers.unwrap_or(8),
                concurrency: options.concurrency.filter(|n| *n > 0),
                poll_interval: Duration::from_millis(options.interval.unwrap_or(1000)),
                prefetch: options.prefetch.unwrap_or(8),
                timeout: options.timeout.unwrap_or(3000),
            },
            shutdown_token: CancellationToken::new(),
        };
        Arc::new(state)
    }
}

/// `hostname:ulid`, also used as the Postgres `application_name`.
#[must_use]
pub fn new_instance_id() -> String {
    let hostname = whoami::hostname().unwrap_or_else(|_| "unknown".into());
    format!("{}:{}", hostname, ulid::Ulid::generate())
}
