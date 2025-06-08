use bytes::Bytes;
use dotenvy::dotenv;
use http_body_util::Full;
use hyper_tls::HttpsConnector;
use hyper_util::client::legacy::{connect::HttpConnector, Client};
use hyper_util::rt::TokioExecutor;
use sqlx::{
    postgres::{PgConnectOptions, PgPoolOptions},
    Pool, Postgres,
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
    pub poll_interval: Duration,
    pub prefetch: u16,
    pub timeout: u32,
}

impl AppState {
    #[must_use]
    pub async fn new() -> Arc<AppState> {
        let flags = xflags::parse_or_exit! {
            /// API Port number. Default: 8102
            optional -p,--port n:u16
            /// Workers count. Default: 8
            optional -w,--workers n:usize
            /// Worker Poll Interval in milliseconds. Default: 1000
            optional --interval n:u64
            /// Prefetch count. Default: 8
            optional --prefetch n:u16
            /// Default job timeout in milliseconds. Default: 3000
            optional --timeout n:u32
        };

        dotenv().ok();
        let hostname = whoami::fallible::hostname().expect("Unable to get hostname");
        let instance_id = format!("{}:{}", hostname, ulid::Ulid::new());
        let db_url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");
        let conn = PgConnectOptions::from_str(&db_url)
            .expect("Unable to parse DATABASE_URL")
            .application_name(&instance_id);

        let pool = PgPoolOptions::new()
            .max_connections(128)
            .connect_with(conn)
            .await
            .expect("Unable to connect to Postgres");

        let https = hyper_tls::HttpsConnector::new();
        let state = AppState {
            port: flags.port.unwrap_or(8102),
            instance_id,
            pool,
            client: Client::builder(TokioExecutor::new()).build::<_, Full<Bytes>>(https),
            scheduler_options: Some(SchedulerOptions {
                poll_interval: Duration::from_millis(5000),
                prefetch: 1000,
            }),
            worker_options: WorkerOptions {
                workers_count: flags.workers.unwrap_or(8),
                poll_interval: Duration::from_millis(flags.interval.unwrap_or(1000)),
                prefetch: flags.prefetch.unwrap_or(8),
                timeout: flags.timeout.unwrap_or(3000),
            },
            shutdown_token: CancellationToken::new(),
        };
        Arc::new(state)
    }
}
