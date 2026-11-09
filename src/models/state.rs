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

use super::{AllowTargets, ApiTokens, Environment, TargetPolicy, targets::GuardedResolver};

//type DbPool = Pool<Postgres>;
#[derive(Debug)]
pub struct AppState {
    pub environment: Environment,
    /// Open the API (`/to/*`, `/api/v1/*`) and the UI; `None`: both are open.
    pub api_tokens: Option<ApiTokens>,
    pub port: u16,
    pub instance_id: String,
    pub pool: Pool<Postgres>,
    pub client: Client<HttpsConnector<HttpConnector<GuardedResolver>>, Full<Bytes>>,
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
    /// Worker tasks; `workers * prefetch` jobs are in flight in all.
    pub workers: usize,
    pub poll_interval: Duration,
    /// Jobs a worker holds and runs at once (and claims in one fetch).
    pub prefetch: u16,
    pub timeout: u32,
    /// Upper bound for the per-job `_timeout`, in milliseconds.
    pub max_timeout: u32,
    /// On shutdown, how long the running jobs may finish; the rest are released.
    pub shutdown_timeout: Duration,
    /// The addresses a job may call.
    pub targets: Arc<TargetPolicy>,
}

/// Extra time on top of `max_timeout` before a lock counts as stuck.
const LOCK_GRACE: Duration = Duration::from_secs(60);
const DEFAULT_DB_CONNECTIONS: u32 = 32;
const DEFAULT_SCHEMA: &str = "irisqo";

// Generates `pub struct Options`; `Options::from_vec(vec![])` gives the defaults (tests).
xflags::xflags! {
    /// Queue of asynchronous HTTP requests with delays, retries and schedules.
    cmd options {
        /// API Port number. Default: 8102
        optional -p,--port n: u16
        /// Max db connections (keep below the Postgres max_connections). Default: 32
        optional --db-connections n: u32
        /// Postgres schema of the tables and functions, created when missing. Default: irisqo
        optional --db-schema name: String
        /// Worker tasks; each holds and runs up to --prefetch jobs at once. Default: 8
        optional -w,--workers n: usize
        /// Worker Poll Interval in milliseconds. Default: 1000
        optional --interval n: u64
        /// Jobs a worker holds and runs at once; in flight in all: workers x prefetch. Default: 8
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
        /// Private addresses and networks a job may call anyway, comma separated:
        /// 10.1.0.0/16,192.168.1.10. Default: none (development allows all)
        optional --allow-targets list: AllowTargets
        /// Bearer tokens for /to/* and /api/v1/*, comma separated, at least 16 characters
        /// each (better set by IRISQO_API_TOKEN: a flag shows in ps). Default: none, open
        optional --api-token list: ApiTokens
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
                        // no value: it may be a secret (IRISQO_API_TOKEN)
                        .map_err(|err| format!("{name}: {err}"))?,
                );
            }
            Ok(())
        }
        // every option, so that a new one cannot miss its variable
        let Options {
            port,
            db_connections,
            db_schema,
            workers,
            interval,
            prefetch,
            timeout,
            max_timeout,
            shutdown_timeout,
            retention_hours,
            trace_url,
            env,
            allow_targets,
            api_token,
        } = self;
        take(port, "IRISQO_PORT", &var)?;
        take(db_connections, "IRISQO_DB_CONNECTIONS", &var)?;
        take(db_schema, "IRISQO_DB_SCHEMA", &var)?;
        take(workers, "IRISQO_WORKERS", &var)?;
        take(interval, "IRISQO_INTERVAL", &var)?;
        take(prefetch, "IRISQO_PREFETCH", &var)?;
        take(timeout, "IRISQO_TIMEOUT", &var)?;
        take(max_timeout, "IRISQO_MAX_TIMEOUT", &var)?;
        take(shutdown_timeout, "IRISQO_SHUTDOWN_TIMEOUT", &var)?;
        take(retention_hours, "IRISQO_RETENTION_HOURS", &var)?;
        take(trace_url, "IRISQO_TRACE_URL", &var)?;
        take(env, "IRISQO_ENV", &var)?;
        take(allow_targets, "IRISQO_ALLOW_TARGETS", &var)?;
        take(api_token, "IRISQO_API_TOKEN", &var)?;
        if let Some(schema) = db_schema {
            check_schema(schema)?;
        }
        Ok(())
    }

    pub fn environment(&self) -> Environment {
        self.env.unwrap_or_default()
    }
}

impl AppState {
    /// Connects to `DATABASE_URL`, creates the schema (`--schema`, `irisqo` by default) and
    /// runs the migrations in it.
    #[must_use]
    pub async fn new(options: &Options) -> Arc<AppState> {
        let instance_id = new_instance_id();
        let schema = options.db_schema.as_deref().unwrap_or(DEFAULT_SCHEMA);
        check_schema(schema).expect("invalid schema");
        let db_url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");
        let conn = PgConnectOptions::from_str(&db_url)
            .expect("Unable to parse DATABASE_URL")
            .application_name(&instance_id)
            // every statement and every function sees the tables of the schema
            .options([("search_path", schema)]);

        let db_connections = options.db_connections.unwrap_or(DEFAULT_DB_CONNECTIONS);
        let pool = PgPoolOptions::new()
            .max_connections(db_connections)
            .acquire_timeout(Duration::from_secs(10))
            .connect_with(conn)
            .await
            .expect("Unable to connect to Postgres");

        // the migrations and their table `_sqlx_migrations` go to the first schema of the path
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "CREATE SCHEMA IF NOT EXISTS \"{schema}\""
        )))
        .execute(&pool)
        .await
        .expect("Unable to create the schema");
        sqlx::migrate!("./sql/migrations")
            .run(&pool)
            .await
            .expect("Unable to run migrations");
        warn_on_clock_skew(&pool).await;

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
        let targets = Arc::new(TargetPolicy::new(
            options.environment(),
            options.allow_targets.clone().unwrap_or_default(),
        ));
        // host names resolve only to the allowed addresses
        let mut http = HttpConnector::new_with_resolver(GuardedResolver::new(Arc::clone(&targets)));
        http.enforce_http(false);
        let https = HttpsConnector::new_with_connector(http);
        let state = AppState {
            environment: options.environment(),
            api_tokens: options.api_token.clone(),
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
                workers: options.workers.unwrap_or(8).max(1),
                poll_interval: Duration::from_millis(options.interval.unwrap_or(1000)),
                prefetch: options.prefetch.unwrap_or(8),
                timeout: options.timeout.unwrap_or(3000).min(max_timeout),
                max_timeout,
                shutdown_timeout: Duration::from_secs(options.shutdown_timeout.unwrap_or(20)),
                targets,
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

/// A schema name that is safe to quote into SQL: lower case letters, digits and `_`.
fn check_schema(name: &str) -> Result<(), String> {
    let mut chars = name.chars();
    let valid = name.len() <= 63
        && chars
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if valid && !name.starts_with("pg_") {
        Ok(())
    } else {
        Err(format!(
            "--db-schema {name:?}: use up to 63 characters of a-z, 0-9 and _, starting with a letter or _, not pg_*"
        ))
    }
}

#[test]
fn schema_names() {
    for ok in ["irisqo", "_q", "queue_2", "a"] {
        assert!(check_schema(ok).is_ok(), "{ok}");
    }
    for bad in [
        "",
        "Irisqo",
        "2q",
        "a-b",
        "a\"b",
        "pg_catalog",
        "public;drop",
    ] {
        assert!(check_schema(bad).is_err(), "{bad}");
    }
}

/// Job times come from the clock of the service but are compared with `now()` of the database:
/// warns once at the start when the clocks differ.
async fn warn_on_clock_skew(pool: &Pool<Postgres>) {
    const MAX_SKEW: chrono::TimeDelta = chrono::TimeDelta::seconds(1);
    let Ok(db_now) = sqlx::query_scalar::<_, chrono::DateTime<chrono::Utc>>("SELECT now()")
        .fetch_one(pool)
        .await
    else {
        return;
    };
    let skew = db_now - chrono::Utc::now();
    if skew.abs() > MAX_SKEW {
        tracing::warn!(
            "the clock of the database differs from this host by {} s: delays and schedules are off by that much",
            skew.num_seconds()
        );
    }
}

/// `hostname:ulid`, also used as the Postgres `application_name`.
#[must_use]
pub fn new_instance_id() -> String {
    // a valid name: lower case, a-z 0-9 . _ : -
    let hostname = whoami::hostname()
        .unwrap_or_else(|_| "unknown".into())
        .to_lowercase()
        .replace(
            |c: char| !(c.is_ascii_alphanumeric() || ".-_".contains(c)),
            "-",
        );
    format!(
        "{}:{}",
        hostname.trim_start_matches(['.', '_', '-']),
        ulid::Ulid::generate().to_string().to_lowercase()
    )
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
    assert_eq!(options.workers, None);

    let mut bad = Options::from_vec(vec![]).expect("options");
    let err = bad
        .fill_from(|name| (name == "IRISQO_PORT").then(|| "port".to_owned()))
        .expect_err("not a number");
    assert!(err.starts_with("IRISQO_PORT: "), "{err}");
}

#[test]
fn a_bad_token_is_not_echoed() {
    let secret = "s3cr3t-short";
    let err = Options::from_vec(vec![])
        .expect("options")
        .fill_from(|name| (name == "IRISQO_API_TOKEN").then(|| secret.to_owned()))
        .expect_err("too short");
    assert!(
        err.starts_with("IRISQO_API_TOKEN:") && !err.contains(secret),
        "{err}"
    );

    let err = Options::from_vec(vec!["--api-token".into(), secret.into()]).expect_err("too short");
    assert!(!err.to_string().contains(secret), "{err}");
}
