use bytes::Bytes;
use http_body_util::Full;
use clap::Parser;
use dotenv::dotenv;
use hyper_util::client::legacy::{Client, connect::HttpConnector};
use hyper_util::rt::TokioExecutor;
use hyper_tls::HttpsConnector;
use sqlx::{
    postgres::{PgConnectOptions, PgPoolOptions},
    Pool, Postgres,
};
use std::{str::FromStr, sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;

#[derive(Parser, Default, Debug)]
pub struct Args {
    #[arg(short = 'p')]
    port: Option<u16>,
    #[arg(short = 'w')]
    workers: Option<usize>,
}

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
        let args = Args::parse();
        dotenv().ok();
        let hostname = whoami::hostname();
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
            port: args.port.unwrap_or(8102),
            instance_id,
            pool,
            client: Client::builder(TokioExecutor::new()).build::<_, Full<Bytes>>(https),
            scheduler_options: Some(SchedulerOptions {
                poll_interval: Duration::from_millis(5000),
                prefetch: 1000,
            }),
            worker_options: WorkerOptions {
                workers_count: args.workers.unwrap_or(4),
                poll_interval: Duration::from_millis(500),
                prefetch: 10,
                timeout: 3000,
            },
            shutdown_token: CancellationToken::new(),
        };
        Arc::new(state)
    }
}
