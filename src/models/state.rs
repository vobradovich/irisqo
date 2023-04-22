use dotenv::dotenv;
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
}

impl AppState {
    pub async fn new() -> Arc<AppState> {
        dotenv().ok();
        let hostname = whoami::hostname();
        let instance_id = format!("{}:1", hostname);
        let db_url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");
        let conn = PgConnectOptions::from_str(&db_url)
            .expect("Unable to parse DATABASE_URL")
            .application_name(&instance_id);

        let pool = PgPoolOptions::new()
            .max_connections(128)
            .connect_with(conn)
            .await
            .expect("Unable to connect to Postgres");

        let state = AppState {
            instance_id,
            pool,
            scheduler_options: Some(SchedulerOptions {
                poll_interval: Duration::from_millis(5000),
                prefetch: 1000,
            }),
            worker_options: WorkerOptions {
                workers_count: Some(10),
                poll_interval: Duration::from_millis(100),
                prefetch: 10,
            },
            shutdown_token: CancellationToken::new(),
        };
        Arc::new(state)
    }
}
