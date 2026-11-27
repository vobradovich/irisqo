use bytes::Bytes;
use http_body_util::Full;
use hyper_util::client::legacy::{Client, connect::HttpConnector};
use hyper_util::rt::TokioExecutor;
use sqlx::{Pool, Postgres};
use std::sync::Arc;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use super::{Held, Options, TargetPolicy, targets::GuardedResolver};
use crate::db::setup;

#[cfg(not(any(feature = "rustls", feature = "native-tls")))]
compile_error!("enable a TLS backend: feature `rustls` (default) or `native-tls`");

/// The TLS of the HTTP client; `rustls` wins when both features are on (e.g. `--all-features`).
#[cfg(feature = "rustls")]
type HttpsConnector<C> = hyper_rustls::HttpsConnector<C>;
#[cfg(all(feature = "native-tls", not(feature = "rustls")))]
type HttpsConnector<C> = hyper_tls::HttpsConnector<C>;

/// # Panics
/// With `rustls`, when `ca_file` is unreadable or holds no certificate, or when no root at all is found.
fn https_connector(
    http: HttpConnector<GuardedResolver>,
    ca_file: Option<&std::path::Path>,
) -> HttpsConnector<HttpConnector<GuardedResolver>> {
    #[cfg(feature = "rustls")]
    {
        use rustls::pki_types::{CertificateDer, pem::PemObject};
        // the system store (rustls-native-certs honours SSL_CERT_FILE / SSL_CERT_DIR) and --ca-file
        let mut roots = rustls::RootCertStore::empty();
        let native = rustls_native_certs::load_native_certs();
        roots.add_parsable_certificates(native.certs);
        if let Some(path) = ca_file {
            let pem =
                std::fs::read(path).unwrap_or_else(|e| panic!("--ca-file {}: {e}", path.display()));
            let certs: Vec<_> = CertificateDer::pem_slice_iter(&pem)
                .collect::<Result<_, _>>()
                .unwrap_or_else(|e| panic!("--ca-file {}: {e}", path.display()));
            assert!(
                !certs.is_empty(),
                "--ca-file {}: no certificate",
                path.display()
            );
            roots.add_parsable_certificates(certs);
        }
        assert!(
            !roots.is_empty(),
            "no trusted CA certificates: set SSL_CERT_FILE or --ca-file"
        );
        let config = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .expect("tls versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
        hyper_rustls::HttpsConnectorBuilder::new()
            .with_tls_config(config)
            .https_or_http()
            .enable_http1()
            .wrap_connector(http)
    }
    #[cfg(all(feature = "native-tls", not(feature = "rustls")))]
    {
        let _ = ca_file;
        HttpsConnector::new_with_connector(http)
    }
}

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
    /// Ends the pause of the worker loader of this process: see [`AppState::wake_workers`].
    pub wake: Notify,
    /// The claimed jobs the worker holds: the heartbeat renews their leases.
    pub held: Held,
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
        // with --no-migrate the schema and the migrations are `irisqo migrations run`'s job
        if !options.no_migrate {
            setup::migrate_schema(&pool, &options.db).await;
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
        let https = https_connector(http, options.ca_file.as_deref());
        Arc::new(AppState {
            options,
            instance_id,
            pool,
            client: Client::builder(TokioExecutor::new()).build::<_, Full<Bytes>>(https),
            targets,
            shutdown_token: CancellationToken::new(),
            wake: Notify::new(),
            held: Held::default(),
        })
    }

    /// A job was queued by the API, the UI or the scheduler: the worker of this process polls
    /// at once (off with `--no-wake`). The worker itself notifies `wake` directly.
    pub fn wake_workers(&self) {
        if self.options.worker.wake {
            self.wake.notify_one();
        }
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
