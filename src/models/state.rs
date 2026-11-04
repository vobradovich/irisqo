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
use std::{fmt::Display, str::FromStr, sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;

use super::Environment;

//type DbPool = Pool<Postgres>;
#[derive(Debug)]
pub struct AppState {
    pub environment: Environment,
    pub port: u16,
    pub instance_id: String,
    pub pool: Pool<Postgres>,
    pub client: Client<HttpsConnector<HttpConnector>, Full<Bytes>>,
    pub scheduler_options: Option<SchedulerOptions>,
    pub worker_options: WorkerOptions,
    /// Link to a trace in the UI: `{trace_id}` is replaced, e.g.
    /// `http://localhost:5080/web/traces?trace_id={trace_id}`.
    pub trace_url: Option<String>,
    pub shutdown_token: CancellationToken,
}

#[derive(Debug)]
pub struct SchedulerOptions {
    pub poll_interval: Duration,
    /// Max jobs moved from `scheduled` to `enqueued` by one statement.
    pub prefetch: u16,
    /// A job locked for this long is released whatever instance holds it
    /// (the result write failed, the worker hung): `max_timeout` + a grace period.
    pub lock_lease: Duration,
    /// Processed jobs older than this are deleted; `0` keeps them forever.
    pub retention_hours: u32,
}

#[derive(Debug)]
pub struct WorkerOptions {
    /// Jobs in flight across all cores.
    pub concurrency: usize,
    pub poll_interval: Duration,
    /// Jobs assigned by one fetch: `max(prefetch, concurrency / 4)`.
    pub prefetch: u16,
    pub timeout: u32,
    /// Upper bound for the per-job `_timeout`, in milliseconds.
    pub max_timeout: u32,
    /// On shutdown, how long the running jobs may finish; the rest are released.
    pub shutdown_timeout: Duration,
}

/// Extra time on top of `max_timeout` before a lock counts as stuck.
const LOCK_GRACE: Duration = Duration::from_secs(60);
const DEFAULT_DB_CONNECTIONS: u32 = 32;

// Generates `pub struct Options`; `Options::from_vec(vec![])` gives the defaults (tests).
xflags::xflags! {
    /// Queue of asynchronous HTTP requests with delays, retries and schedules.
    cmd options {
        /// API Port number. Default: 8102
        optional -p,--port n: u16
        /// Max db connections (keep below the Postgres max_connections). Default: 32
        optional --db-connections n: u32
        /// Jobs in flight across all cores. Default: 64
        optional --concurrency n: usize
        /// Worker Poll Interval in milliseconds. Default: 1000
        optional --interval n: u64
        /// Prefetch count. Default: 8
        optional --prefetch n: u16
        /// Default job timeout in milliseconds. Default: 3000
        optional --timeout n: u32
        /// Max job timeout (`_timeout`) in milliseconds. Default: 300000
        optional --max-timeout n: u32
        /// Seconds the running jobs get to finish on shutdown, then they are released
        /// (keep it under the orchestrator grace period, 30 s in Kubernetes). Default: 20
        optional --shutdown-timeout n: u64
        /// Keep processed jobs for this many hours, 0 keeps them forever. Default: 168 (7 days)
        optional --retention-hours n: u32
        /// UI link to a trace, `{trace_id}` is replaced. Default: none (the id is shown)
        optional --trace-url template: String
        /// development, staging or production: development serves /echo and /error and
        /// allows private targets. Default: production
        optional --env name: Environment
    }
}

impl Options {
    /// The command line, then the `IRISQO_*` variables (the environment or a `.env` file)
    /// for the options it does not give: `--max-timeout` falls back to `IRISQO_MAX_TIMEOUT`.
    /// Exits with a message on a bad value, like a bad flag.
    #[must_use]
    pub fn load() -> Options {
        dotenv().ok();
        let mut options = Options::from_env_or_exit();
        if let Err(err) = options.fill_from(|name| std::env::var(name).ok()) {
            eprintln!("error: {err}");
            std::process::exit(2);
        }
        options
    }

    /// Fills the options not given on the command line from `var("IRISQO_<OPTION>")`.
    pub fn fill_from(&mut self, var: impl Fn(&str) -> Option<String>) -> Result<(), String> {
        fn take<T>(
            option: &mut Option<T>,
            name: &str,
            var: &impl Fn(&str) -> Option<String>,
        ) -> Result<(), String>
        where
            T: FromStr,
            T::Err: Display,
        {
            if option.is_none()
                && let Some(value) = var(name)
            {
                *option = Some(
                    value
                        .parse()
                        .map_err(|err| format!("{name}={value}: {err}"))?,
                );
            }
            Ok(())
        }
        // every option, so that a new one cannot miss its variable
        let Options {
            port,
            db_connections,
            concurrency,
            interval,
            prefetch,
            timeout,
            max_timeout,
            shutdown_timeout,
            retention_hours,
            trace_url,
            env,
        } = self;
        take(port, "IRISQO_PORT", &var)?;
        take(db_connections, "IRISQO_DB_CONNECTIONS", &var)?;
        take(concurrency, "IRISQO_CONCURRENCY", &var)?;
        take(interval, "IRISQO_INTERVAL", &var)?;
        take(prefetch, "IRISQO_PREFETCH", &var)?;
        take(timeout, "IRISQO_TIMEOUT", &var)?;
        take(max_timeout, "IRISQO_MAX_TIMEOUT", &var)?;
        take(shutdown_timeout, "IRISQO_SHUTDOWN_TIMEOUT", &var)?;
        take(retention_hours, "IRISQO_RETENTION_HOURS", &var)?;
        take(trace_url, "IRISQO_TRACE_URL", &var)?;
        take(env, "IRISQO_ENV", &var)?;
        Ok(())
    }

    pub fn environment(&self) -> Environment {
        self.env.unwrap_or_default()
    }
}

impl AppState {
    /// Connects to `DATABASE_URL` and runs the migrations.
    #[must_use]
    pub async fn new(options: &Options) -> Arc<AppState> {
        let instance_id = new_instance_id();
        let db_url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");
        let conn = PgConnectOptions::from_str(&db_url)
            .expect("Unable to parse DATABASE_URL")
            .application_name(&instance_id);

        let db_connections = options.db_connections.unwrap_or(DEFAULT_DB_CONNECTIONS);
        let pool = PgPoolOptions::new()
            .max_connections(db_connections)
            .acquire_timeout(Duration::from_secs(10))
            .connect_with(conn)
            .await
            .expect("Unable to connect to Postgres");

        sqlx::migrate!("./sql/migrations")
            .run(&pool)
            .await
            .expect("Unable to run migrations");

        Self::with_pool(pool, instance_id, options)
    }

    /// Builds the state on top of an already connected (and migrated) pool.
    #[must_use]
    pub fn with_pool(
        pool: Pool<Postgres>,
        instance_id: String,
        options: &Options,
    ) -> Arc<AppState> {
        let max_timeout = options.max_timeout.unwrap_or(300_000);
        let https = hyper_tls::HttpsConnector::new();
        let state = AppState {
            environment: options.environment(),
            port: options.port.unwrap_or(8102),
            instance_id,
            pool,
            client: Client::builder(TokioExecutor::new()).build::<_, Full<Bytes>>(https),
            scheduler_options: Some(SchedulerOptions {
                poll_interval: Duration::from_millis(5000),
                prefetch: 1000,
                lock_lease: Duration::from_millis(u64::from(max_timeout)) + LOCK_GRACE,
                retention_hours: options.retention_hours.unwrap_or(7 * 24),
            }),
            worker_options: WorkerOptions {
                concurrency: options.concurrency.unwrap_or(64).max(1),
                poll_interval: Duration::from_millis(options.interval.unwrap_or(1000)),
                prefetch: options.prefetch.unwrap_or(8),
                timeout: options.timeout.unwrap_or(3000).min(max_timeout),
                max_timeout,
                shutdown_timeout: Duration::from_secs(options.shutdown_timeout.unwrap_or(20)),
            },
            trace_url: options
                .trace_url
                .clone()
                .filter(|t| t.contains("{trace_id}")),
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

#[test]
fn options_fall_back_to_the_environment() {
    let vars = |name: &str| match name {
        "IRISQO_PORT" => Some("9000".to_owned()),
        "IRISQO_MAX_TIMEOUT" => Some("1000".to_owned()),
        "IRISQO_ENV" => Some("dev".to_owned()),
        _ => None,
    };
    let mut options = Options::from_vec(vec!["--port".into(), "8000".into()]).expect("options");
    options.fill_from(vars).expect("valid");
    assert_eq!(options.port, Some(8000), "the command line wins");
    assert_eq!(options.max_timeout, Some(1000));
    assert_eq!(options.environment(), Environment::Development);
    assert_eq!(options.concurrency, None);

    let mut bad = Options::from_vec(vec![]).expect("options");
    let err = bad
        .fill_from(|name| (name == "IRISQO_PORT").then(|| "port".to_owned()))
        .expect_err("not a number");
    assert!(err.starts_with("IRISQO_PORT=port"), "{err}");
}
