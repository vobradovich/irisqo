//! Settings: the options of this instance as it runs, read-only. A secret is never shown: the
//! database by its host and name only, the API tokens and the signing keys by their number.

use std::{sync::Arc, time::Duration};

use askama::Template;
use askama_web::WebTemplate;
use axum::extract::State;

use super::Nav;
use crate::models::{AppState, Options};

/// One read-only field of the page.
pub struct Field {
    pub label: &'static str,
    pub value: String,
    pub hint: String,
    /// Shown with a lock: hidden or reduced on purpose.
    pub secret: bool,
    pub mono: bool,
    /// The whole row.
    pub wide: bool,
}

impl Field {
    fn new(label: &'static str, value: impl Into<String>) -> Self {
        Field {
            label,
            value: value.into(),
            hint: String::new(),
            secret: false,
            mono: false,
            wide: false,
        }
    }

    fn hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = hint.into();
        self
    }

    fn secret(mut self) -> Self {
        self.secret = true;
        self
    }

    fn mono(mut self) -> Self {
        self.mono = true;
        self
    }

    fn wide(mut self) -> Self {
        self.wide = true;
        self
    }
}

/// What the page shows of [`Options`].
pub struct SettingsView {
    pub port: String,
    pub environment: &'static str,
    pub roles: Vec<&'static str>,
    /// `host:port / name`: no user, no password, no query.
    pub database: String,
    pub database_hint: String,
    pub access: Vec<Field>,
    pub limits: Vec<Field>,
    pub worker: Vec<Field>,
    pub scheduler: Vec<Field>,
    pub network: Vec<Field>,
}

/// The host, the port and the name of a Postgres URL: the rest may hold a password.
fn database_label(url: &str) -> String {
    let Ok(url) = url::Url::parse(url) else {
        return "hidden".into();
    };
    let host = url.host_str().unwrap_or("local socket");
    let port = url.port().map(|p| format!(":{p}")).unwrap_or_default();
    format!("{host}{port} / {}", url.path().trim_start_matches('/'))
}

fn bytes(n: usize) -> String {
    format!("{n} B")
}

fn kib(n: usize) -> String {
    if n >= 1024 && n.is_multiple_of(1024) {
        format!("{} KiB.", n / 1024)
    } else {
        String::new()
    }
}

fn duration(d: Duration) -> String {
    let ms = d.as_millis();
    if ms >= 1000 && ms.is_multiple_of(1000) {
        format!("{} s", ms / 1000)
    } else {
        format!("{ms} ms")
    }
}

fn count(n: usize, what: &str) -> String {
    format!("{n} {what}{} set", if n == 1 { "" } else { "s" })
}

impl SettingsView {
    pub fn new(options: &Options) -> Self {
        let worker = &options.worker;
        let scheduler = &options.scheduler;
        let tokens = options.api_tokens.as_ref().map_or(0, |t| t.count());
        let keys = worker.signing_keys.as_ref().map_or(0, |k| k.count());
        let retention = match scheduler.retention_hours {
            0 => "0 keeps everything.".to_owned(),
            h if h.is_multiple_of(24) => format!("{} days; 0 keeps everything.", h / 24),
            _ => "0 keeps everything.".to_owned(),
        };
        SettingsView {
            port: options.port.to_string(),
            environment: options.environment.as_str(),
            roles: options.effective_roles().names(),
            database: database_label(&options.db.url),
            database_hint: format!(
                "schema {} · pool {} · user and password hidden",
                options.db.schema, options.db.connections
            ),
            access: vec![
                Field::new(
                    "API tokens",
                    if tokens == 0 {
                        "not set".to_owned()
                    } else {
                        count(tokens, "token")
                    },
                )
                .hint(if tokens == 0 {
                    "The API and the UI are open."
                } else {
                    "The API and the UI ask for a token."
                })
                .secret(),
                Field::new(
                    "Callback signing keys",
                    if keys == 0 {
                        "not set".to_owned()
                    } else {
                        count(keys, "key")
                    },
                )
                .hint(if keys == 0 {
                    "The callbacks are not signed."
                } else {
                    "The callbacks are signed."
                })
                .secret(),
            ],
            limits: vec![
                Field::new("Default timeout", format!("{} ms", worker.timeout)),
                Field::new("Max timeout", format!("{} ms", worker.max_timeout)),
                Field::new("Max response size", bytes(worker.max_response_size))
                    .hint(kib(worker.max_response_size)),
                Field::new("Max body size", bytes(options.max_body_size))
                    .hint(kib(options.max_body_size)),
                Field::new("Retention", format!("{} h", scheduler.retention_hours)).hint(retention),
                Field::new(
                    "Trace link",
                    options.trace_url.as_deref().unwrap_or("not set"),
                )
                .mono()
                .wide(),
            ],
            worker: vec![
                Field::new("Workers", worker.workers.to_string()),
                Field::new("Prefetch", worker.prefetch.to_string())
                    .hint("Jobs a worker runs at once."),
                Field::new("Poll interval", duration(worker.poll_interval)),
                Field::new("Max poll interval", duration(worker.max_poll_interval)),
                Field::new("Wake", if worker.wake { "on" } else { "off" })
                    .hint("A job queued by the API, the UI or the scheduler wakes the worker."),
                Field::new("Shutdown timeout", duration(worker.shutdown_timeout)),
                Field::new("Claim lease", duration(crate::models::CLAIM_LEASE))
                    .hint("Renewed by the heartbeat while the worker holds the job."),
            ],
            scheduler: vec![
                Field::new("Poll interval", duration(scheduler.poll_interval)),
                Field::new("Prefetch", scheduler.prefetch.to_string()),
            ],
            network: vec![
                Field::new(
                    "Allowed private targets",
                    if options.allow_targets.0.is_empty() {
                        "none".to_owned()
                    } else {
                        options
                            .allow_targets
                            .0
                            .iter()
                            .map(ToString::to_string)
                            .collect::<Vec<_>>()
                            .join(", ")
                    },
                ),
                Field::new(
                    "CA file",
                    options
                        .ca_file
                        .as_ref()
                        .map_or("not set".to_owned(), |p| p.display().to_string()),
                ),
                Field::new("Pool size", options.db.connections.to_string()),
                Field::new(
                    "Migrations at start",
                    if options.no_migrate { "no" } else { "yes" },
                ),
            ],
        }
    }
}

#[derive(Template, WebTemplate)]
#[template(path = "ui/settings.html")]
pub struct SettingsPage {
    nav: Nav,
    view: SettingsView,
}

pub async fn settings_page(State(state): State<Arc<AppState>>) -> SettingsPage {
    SettingsPage {
        nav: Nav::new(&state, "settings"),
        view: SettingsView::new(&state.options),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every text of the page, to look for what must not be on it.
    fn everything(view: &SettingsView) -> String {
        let fields = [
            &view.access,
            &view.limits,
            &view.worker,
            &view.scheduler,
            &view.network,
        ];
        let mut text = vec![
            view.port.clone(),
            view.database.clone(),
            view.database_hint.clone(),
        ];
        for field in fields.into_iter().flatten() {
            text.extend([field.value.clone(), field.hint.clone()]);
        }
        text.join("\n")
    }

    #[test]
    fn the_database_is_shown_without_its_credentials() {
        assert_eq!(
            database_label("postgres://alice_db:s3cr3t@db.internal:5433/queue?sslpassword=hush"),
            "db.internal:5433 / queue"
        );
        assert_eq!(database_label("not a url"), "hidden");
    }

    #[test]
    fn no_secret_is_on_the_page() {
        let mut options =
            Options::new("postgres://alice_db:s3cr3t@db.internal/queue?password=hush");
        options.api_tokens = Some(
            "0123456789abcdef0123,fedcba9876543210fedc"
                .parse()
                .expect("tokens"),
        );
        options.worker.signing_keys = Some(
            "whsec_MfKQ9r8GKYqrTwjBZ8NQvxhy3mTrtmQ1yuUnxkc4PqE="
                .parse()
                .expect("key"),
        );
        let text = everything(&SettingsView::new(&options));
        for secret in [
            "s3cr3t",
            "hush",
            "alice_db",
            "0123456789abcdef0123",
            "fedcba9876543210fedc",
            "whsec_",
            "MfKQ9r8G",
        ] {
            assert!(!text.contains(secret), "{secret} must not be shown");
        }
        assert!(text.contains("2 tokens set") && text.contains("1 key set"));
        assert!(text.contains("db.internal / queue"));
    }
}
