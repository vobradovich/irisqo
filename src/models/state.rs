use dotenv::dotenv;
use hyper::{client::HttpConnector, Body, Client};
use hyper_tls::HttpsConnector;
use sqlx::{
    postgres::{PgConnectOptions, PgPoolOptions},
    Pool, Postgres,
};
use std::{str::FromStr, sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;

//type DbPool = Pool<Postgres>;
#[derive(Debug)]
pub struct AppState {
    pub instance_id: String,
    pub pool: Pool<Postgres>,
    pub client: Client<HttpsConnector<HttpConnector>, Body>,
    pub scheduler_options: Option<SchedulerOptions>,
    pub worker_options: WorkerOptions,
    pub shutdown_token: CancellationToken,
}

#[derive(Debug)]
pub struct SchedulerOptions {
    pub poll_interval: Duration,
    pub prefetch: i32,
}

#[derive(Debug)]
pub struct WorkerOptions {
    pub workers_count: Option<usize>,
    pub poll_interval: Duration,
    pub prefetch: i32,
    pub timeout: u32,
}

impl AppState {
    #[must_use]
    pub async fn new() -> Arc<AppState> {
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
            instance_id,
            pool,
            client: hyper::Client::builder().build::<_, hyper::Body>(https),
            scheduler_options: Some(SchedulerOptions {
                poll_interval: Duration::from_millis(5000),
                prefetch: 1000,
            }),
            worker_options: WorkerOptions {
                workers_count: Some(4),
                poll_interval: Duration::from_millis(1000),
                prefetch: 10,
                timeout: 3000,
            },
            shutdown_token: CancellationToken::new(),
        };
        Arc::new(state)
    }
}
