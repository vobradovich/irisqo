use bytes::Bytes;
use http_body_util::Full;
use hyper_tls::HttpsConnector;
use hyper_util::client::legacy::{Client, connect::HttpConnector};
use hyper_util::rt::TokioExecutor;
use sqlx::{Pool, Postgres};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use super::{Options, TargetPolicy, targets::GuardedResolver};
use crate::db::setup;

#[derive(Debug)]
pub struct AppState {
    /// What the instance was started with; it does not change.
    pub options: Options,
    pub instance_id: String,
    pub pool: Pool<Postgres>,
    pub client: Client<HttpsConnector<HttpConnector<GuardedResolver>>, Full<Bytes>>,
    /// The addresses a job may call, from `options.environment` and `options.allow_targets`;
    /// the resolver of `client` holds the same policy.
    pub targets: Arc<TargetPolicy>,
    pub shutdown_token: CancellationToken,
}

impl AppState {
    /// Connects to the database of `options`, creates the schema (`irisqo` by default) and
    /// runs the migrations in it, unless `options.no_migrate`.
    ///
    /// # Panics
    /// On options that [`Options::validate`] refuses, and when the database is not reachable.
    #[must_use]
    pub async fn new(options: Options) -> Arc<AppState> {
        options.validate().expect("invalid options");
        let instance_id = new_instance_id();
        let pool = setup::connect(&options.db, &instance_id).await;
        // with --no-migrate the schema and the migrations are `irisqo migrate`'s job
        if !options.no_migrate {
            setup::migrate_schema(&pool, &options.db.schema).await;
        }
        warn_on_clock_skew(&pool).await;

        Self::with_pool(pool, instance_id, options)
    }

    /// Builds the state on top of an already connected (and migrated) pool.
    ///
    /// # Panics
    /// On options that [`Options::validate`] refuses.
    #[must_use]
    pub fn with_pool(pool: Pool<Postgres>, instance_id: String, options: Options) -> Arc<AppState> {
        options.validate().expect("invalid options");
        let targets = Arc::new(TargetPolicy::new(
            options.environment,
            options.allow_targets.clone(),
        ));
        // host names resolve only to the allowed addresses
        let mut http = HttpConnector::new_with_resolver(GuardedResolver::new(Arc::clone(&targets)));
        http.enforce_http(false);
        let https = HttpsConnector::new_with_connector(http);
        Arc::new(AppState {
            options,
            instance_id,
            pool,
            client: Client::builder(TokioExecutor::new()).build::<_, Full<Bytes>>(https),
            targets,
            shutdown_token: CancellationToken::new(),
        })
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

/// A unique name that sorts by time: a lower case ULID.
#[must_use]
pub fn new_ulid() -> String {
    ulid::Ulid::generate().to_string().to_lowercase()
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
        new_ulid()
    )
}
