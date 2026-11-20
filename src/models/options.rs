//! The settings of an instance, independent of how they were read: build [`Options`] by hand
//! (`Options::new(db_url)` and override what differs) or let the command line do it
//! (`irisqo::cli`). The state keeps them as they are, `AppState::options`.

use std::{fmt, time::Duration};

use super::{AllowTargets, ApiTokens, Environment, SigningKeys};

/// Extra time on top of `3 * max_timeout` before a lock counts as stuck.
const LOCK_GRACE: Duration = Duration::from_secs(60);
const DEFAULT_DB_CONNECTIONS: u32 = 32;
pub const DEFAULT_SCHEMA: &str = "irisqo";
const DEFAULT_PORT: u16 = 8102;
const DEFAULT_MAX_BODY_SIZE: usize = 64 * 1024;
const DEFAULT_MAX_RESPONSE_SIZE: usize = 64 * 1024;

/// A setting that cannot work; the message names it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ConfigError(String);

impl ConfigError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

/// Everything an instance is configured with. [`Options::validate`] checks the whole.
#[derive(Debug, Clone)]
pub struct Options {
    pub db: DbOptions,
    /// API port.
    pub port: u16,
    pub environment: Environment,
    /// The longest HTTP request body, in bytes.
    pub max_body_size: usize,
    /// Open the API (`/to/*`, `/api/v1/*`) and the UI; `None`: both are open.
    pub api_tokens: Option<ApiTokens>,
    /// Link to a trace in the UI: `{trace_id}` is replaced, e.g.
    /// `http://localhost:5080/web/traces?trace_id={trace_id}`.
    pub trace_url: Option<String>,
    /// Private addresses and networks a job may call anyway.
    pub allow_targets: AllowTargets,
    /// Do not create the schema or run the migrations at the start: the deploy does it.
    pub no_migrate: bool,
    pub worker: WorkerOptions,
    pub scheduler: SchedulerOptions,
}

impl Options {
    /// The defaults and the database at `db_url`, the only setting that has none.
    #[must_use]
    pub fn new(db_url: impl Into<String>) -> Self {
        Self {
            db: DbOptions::new(db_url),
            port: DEFAULT_PORT,
            environment: Environment::default(),
            max_body_size: DEFAULT_MAX_BODY_SIZE,
            api_tokens: None,
            trace_url: None,
            allow_targets: AllowTargets::default(),
            no_migrate: false,
            worker: WorkerOptions::default(),
            scheduler: SchedulerOptions::default(),
        }
    }

    /// A job locked for this long is released whatever instance holds it (the result write
    /// failed, the worker hung): up to two runs ahead and its own, `3 * max_timeout` and a grace period.
    #[must_use]
    pub fn lock_lease(&self) -> Duration {
        Duration::from_millis(u64::from(self.worker.max_timeout)) * 3 + LOCK_GRACE
    }

    /// The first setting that cannot work.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.db.url.is_empty() {
            return Err(ConfigError::new("the database URL is empty"));
        }
        check_schema(&self.db.schema)?;
        if self.db.connections == 0 {
            return Err(ConfigError::new("db.connections must be at least 1"));
        }
        if self.worker.workers == 0 {
            return Err(ConfigError::new("worker.workers must be at least 1"));
        }
        if self.worker.timeout > self.worker.max_timeout {
            return Err(ConfigError::new(
                "worker.timeout must not exceed worker.max_timeout",
            ));
        }
        if let Some(url) = &self.trace_url
            && !url.contains("{trace_id}")
        {
            return Err(ConfigError::new("trace_url must contain {trace_id}"));
        }
        Ok(())
    }
}

/// The database of the queue.
#[derive(Clone)]
pub struct DbOptions {
    /// `postgres://user:pass@host/db`.
    pub url: String,
    /// Pool size; keep the sum over the instances below the Postgres `max_connections`.
    pub connections: u32,
    /// Postgres schema of the tables and functions, created when missing.
    pub schema: String,
}

impl DbOptions {
    #[must_use]
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            connections: DEFAULT_DB_CONNECTIONS,
            schema: DEFAULT_SCHEMA.to_owned(),
        }
    }
}

/// Not derived: the URL holds the password.
impl fmt::Debug for DbOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // every field named: a new one must be decided on here
        let Self {
            url: _,
            connections,
            schema,
        } = self;
        f.debug_struct("DbOptions")
            .field("url", &"<redacted>")
            .field("connections", connections)
            .field("schema", schema)
            .finish()
    }
}

#[derive(Debug, Clone)]
pub struct SchedulerOptions {
    pub poll_interval: Duration,
    /// Max jobs moved from `scheduled` to `enqueued` by one statement.
    pub prefetch: u16,
    /// Processed jobs older than this are deleted; `0` keeps them forever.
    pub retention_hours: u32,
}

impl Default for SchedulerOptions {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(1),
            prefetch: 1000,
            retention_hours: 7 * 24,
        }
    }
}

#[derive(Debug, Clone)]
pub struct WorkerOptions {
    /// Worker tasks; `workers * prefetch` jobs are in flight in all.
    pub workers: usize,
    /// First pause before polling an empty queue again; it doubles up to `max_poll_interval`.
    pub poll_interval: Duration,
    pub max_poll_interval: Duration,
    /// Jobs a worker holds and runs at once (and claims in one fetch).
    pub prefetch: u16,
    /// The timeout of a job that sets none, in milliseconds; not above `max_timeout`.
    pub timeout: u32,
    /// Upper bound for the per-job `_timeout`, in milliseconds.
    pub max_timeout: u32,
    /// The longest response body a job takes: a longer one fails the job.
    pub max_response_size: usize,
    /// On shutdown, how long the running jobs may finish; the rest are released.
    pub shutdown_timeout: Duration,
    /// Sign the callbacks; `None`: they are not signed.
    pub signing_keys: Option<SigningKeys>,
}

impl Default for WorkerOptions {
    fn default() -> Self {
        Self {
            workers: 16,
            poll_interval: Duration::from_millis(200),
            max_poll_interval: Duration::from_millis(1000),
            prefetch: 4,
            timeout: 3000,
            max_timeout: 300_000,
            max_response_size: DEFAULT_MAX_RESPONSE_SIZE,
            shutdown_timeout: Duration::from_secs(20),
            signing_keys: None,
        }
    }
}

#[cfg(test)]
impl WorkerOptions {
    /// Options for a unit test.
    pub(crate) fn for_tests() -> Self {
        Self {
            workers: 1,
            poll_interval: Duration::from_secs(1),
            max_poll_interval: Duration::from_secs(1),
            prefetch: 1,
            shutdown_timeout: Duration::from_secs(1),
            ..Self::default()
        }
    }
}

/// A schema name that is safe to quote into SQL: lower case letters, digits and `_`.
pub(crate) fn check_schema(name: &str) -> Result<(), ConfigError> {
    let mut chars = name.chars();
    let valid = name.len() <= 63
        && chars
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if valid && !name.starts_with("pg_") {
        Ok(())
    } else {
        Err(ConfigError::new(format!(
            "db schema {name:?}: use up to 63 characters of a-z, 0-9 and _, starting with a letter or _, not pg_*"
        )))
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

#[test]
fn the_defaults_are_valid_and_the_password_is_not_shown() {
    let options = Options::new("postgres://user:s3cr3t@localhost/irisqo");
    options.validate().expect("the defaults work");
    assert!(!format!("{options:?}").contains("s3cr3t"));
    assert_eq!(options.lock_lease(), Duration::from_secs(3 * 300 + 60));
}

#[test]
fn a_setting_that_cannot_work_is_named() {
    let bad = |change: fn(&mut Options)| {
        let mut options = Options::new("postgres://localhost/irisqo");
        change(&mut options);
        options.validate().expect_err("invalid").to_string()
    };
    assert!(bad(|o| o.db.url.clear()).contains("database URL"));
    assert!(bad(|o| o.db.schema = "Public".into()).contains("schema"));
    assert!(bad(|o| o.db.connections = 0).contains("db.connections"));
    assert!(bad(|o| o.worker.workers = 0).contains("worker.workers"));
    assert!(bad(|o| o.worker.timeout = o.worker.max_timeout + 1).contains("max_timeout"));
    assert!(bad(|o| o.trace_url = Some("http://t/".into())).contains("{trace_id}"));
}
