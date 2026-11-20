//! The command line of the binary: flags, then the `IRISQO_*` variables, become the
//! [`Options`] of the library (which can also be built by hand).

use std::{ffi::OsString, fmt::Display, path::PathBuf, str::FromStr, time::Duration};

use dotenvy::dotenv;

use crate::models::{
    AllowTargets, ApiTokens, DEFAULT_SCHEMA, DbOptions, Environment, Options, Roles,
    SchedulerOptions, SigningKeys, WorkerOptions, check_schema,
};

// Generates `Irisqo` and its commands `Serve`, `Run` and `Sql`: the flags as given, all optional.
xflags::xflags! {
    /// Queue of asynchronous HTTP requests with delays, retries and schedules.
    cmd irisqo {
        /// Runs the API, the UI, the workers and the scheduler. The command when none is given.
        default cmd serve {
        /// API Port number. Default: 8102
        optional -p,--port n: u16
        /// Maximum HTTP request body size in bytes. Default: 65536 (64 KiB)
        optional --max-body-size n: usize
        /// Maximum size of a target's response body in bytes; a longer one fails the job (it is
        /// not retried). Default: 65536 (64 KiB)
        optional --max-response-size n: usize
        /// Postgres connection URL, postgres://user:pass@host/db (better set by IRISQO_DB_URL: a flag
        /// shows in ps). Required.
        optional --db-url url: String
        /// Max db connections (keep below the Postgres max_connections). Default: 32
        optional --db-connections n: u32
        /// Postgres schema of the tables and functions, created when missing. Default: irisqo
        optional --db-schema name: String
        /// Worker tasks; each holds and runs up to --prefetch jobs at once. Default: 16
        optional -w,--workers n: usize
        /// Pause before the first re-poll of an empty queue, in milliseconds; it doubles while the queue stays empty. Default: 200
        optional --interval n: u64
        /// Longest pause between polls of an empty queue, in milliseconds. Default: 1000
        optional --max-interval n: u64
        /// Jobs a worker holds and runs at once; in flight in all: workers x prefetch. Default: 4
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
        /// PEM file with CA certificates to trust, besides the system store (SSL_CERT_FILE and
        /// SSL_CERT_DIR are honoured) when calling the targets. For Postgres use
        /// `?sslmode=verify-full&sslrootcert=/path` in the URL. Default: none
        optional --ca-file path: PathBuf
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
        /// Keys that sign the callbacks, comma separated (several: a rotation), each whsec_ and the
        /// base64 of 24 to 64 bytes (better set by IRISQO_SIGNING_KEY: a flag shows in ps).
        /// Default: none, not signed
        optional --signing-key list: SigningKeys
        /// Do not create the schema or run the migrations: apply them first with `irisqo migrations run`.
        optional --no-migrate
        /// What this process does, comma separated: api, ui, worker, scheduler, retention
        /// (retention needs --retention-hours above 0). The probes are always served. At least one
        /// process of the cluster needs the scheduler. Default: all of them
        optional --roles list: Roles
        }
        /// Runs the migrations, or prints them for another tool.
        cmd migrations {
            /// Creates the schema and runs the migrations, then exits: a deploy step, run before the servers start.
            cmd run {
                /// Postgres connection URL, postgres://user:pass@host/db (better set by IRISQO_DB_URL: a flag
                /// shows in ps). Required.
                optional --db-url url: String
                /// Max db connections. Default: 32
                optional --db-connections n: u32
                /// Postgres schema of the tables and functions, created when missing. Default: irisqo
                optional --db-schema name: String
                /// DROPS the schema with all its data (jobs, schedules, history) and creates it anew:
                /// for development and tests. Refused when --env is production, which is the default:
                /// pass --env development or staging.
                optional --recreate
                /// development, staging or production; only --recreate looks at it. Default: production
                optional --env name: Environment
            }
            /// Prints the SQL of the migrations to stdout, to run them with Flyway, Liquibase, EF or
            /// psql instead of `irisqo migrations run` (then start the servers with --no-migrate).
            cmd sql {
                /// Postgres schema the script creates and uses. Default: irisqo
                optional --db-schema name: String
            }
        }
    }
}

/// A bare `irisqo` runs the server; `irisqo migrations run` only applies the migrations.
#[allow(clippy::large_enum_variant)] // built once at start, not moved around
pub enum Command {
    Serve(Options),
    /// The options give the database and the environment; `recreate` drops the schema first.
    MigrationsRun {
        options: Options,
        recreate: bool,
    },
    /// `irisqo migrations sql`: print the SQL of the migrations for `schema`.
    MigrationsSql {
        schema: String,
    },
}

impl Command {
    /// The command line, then the `IRISQO_*` variables (the environment or a `.env` file)
    /// for the options it does not give: `--max-timeout` falls back to `IRISQO_MAX_TIMEOUT`.
    /// Exits with a message on a bad value, like a bad flag.
    #[must_use]
    pub fn load() -> Command {
        dotenv().ok();
        let cli = Irisqo::from_env_or_exit();
        let var = |name: &str| std::env::var(name).ok();
        let loaded = match cli.subcommand {
            IrisqoCmd::Serve(mut serve) => serve
                .fill_from(var)
                .and_then(|()| serve.into_options())
                .map(Command::Serve),
            IrisqoCmd::Migrations(Migrations {
                subcommand: MigrationsCmd::Run(mut run),
            }) => run.fill_from(var).and_then(|()| run.into_command()),
            IrisqoCmd::Migrations(Migrations {
                subcommand: MigrationsCmd::Sql(sql),
            }) => sql.into_command(var),
        };
        loaded.unwrap_or_else(|err| {
            eprintln!("error: {err}");
            std::process::exit(2);
        })
    }
}

/// The options of a bare `irisqo` with `args` (without the program name) and no environment
/// variables: what the tests of the library build the state from.
pub fn options_from_args(args: Vec<OsString>) -> Result<Options, String> {
    match Irisqo::from_vec(args)
        .map_err(|err| err.to_string())?
        .subcommand
    {
        IrisqoCmd::Serve(serve) => serve.into_options(),
        IrisqoCmd::Migrations(_) => Err("not the server command".to_owned()),
    }
}

impl Serve {
    /// Fills the options not given on the command line from `var("IRISQO_<OPTION>")`.
    pub fn fill_from(&mut self, var: impl Fn(&str) -> Option<String>) -> Result<(), String> {
        // every option, so that a new one cannot miss its variable
        let Serve {
            port,
            max_body_size,
            max_response_size,
            db_url,
            db_connections,
            db_schema,
            workers,
            interval,
            max_interval,
            prefetch,
            timeout,
            max_timeout,
            shutdown_timeout,
            retention_hours,
            trace_url,
            ca_file,
            env,
            allow_targets,
            api_token,
            signing_key,
            roles,
            // a flag only: it has no variable
            no_migrate: _,
        } = self;
        take(port, "IRISQO_PORT", &var)?;
        take(max_body_size, "IRISQO_MAX_BODY_SIZE", &var)?;
        take(max_response_size, "IRISQO_MAX_RESPONSE_SIZE", &var)?;
        take(db_url, "IRISQO_DB_URL", &var)?;
        take(db_connections, "IRISQO_DB_CONNECTIONS", &var)?;
        take(db_schema, "IRISQO_DB_SCHEMA", &var)?;
        take(workers, "IRISQO_WORKERS", &var)?;
        take(interval, "IRISQO_INTERVAL", &var)?;
        take(max_interval, "IRISQO_MAX_INTERVAL", &var)?;
        take(prefetch, "IRISQO_PREFETCH", &var)?;
        take(timeout, "IRISQO_TIMEOUT", &var)?;
        take(max_timeout, "IRISQO_MAX_TIMEOUT", &var)?;
        take(shutdown_timeout, "IRISQO_SHUTDOWN_TIMEOUT", &var)?;
        take(retention_hours, "IRISQO_RETENTION_HOURS", &var)?;
        take(trace_url, "IRISQO_TRACE_URL", &var)?;
        take(ca_file, "IRISQO_CA_FILE", &var)?;
        take(env, "IRISQO_ENV", &var)?;
        take(allow_targets, "IRISQO_ALLOW_TARGETS", &var)?;
        take(api_token, "IRISQO_API_TOKEN", &var)?;
        take(signing_key, "IRISQO_SIGNING_KEY", &var)?;
        take(roles, "IRISQO_ROLES", &var)?;
        Ok(())
    }

    /// The flags over the defaults of [`Options::new`], checked.
    ///
    /// The command line is forgiving where the library is strict: a `--timeout` above
    /// `--max-timeout` is lowered to it, no worker becomes one, and a `--trace-url` without
    /// `{trace_id}` is ignored.
    pub fn into_options(self) -> Result<Options, String> {
        let db_url = self.db_url.ok_or("--db-url or IRISQO_DB_URL must be set")?;
        let d = Options::new(db_url);
        let max_timeout = self.max_timeout.unwrap_or(d.worker.max_timeout);
        let options = Options {
            db: DbOptions {
                url: d.db.url,
                connections: self.db_connections.unwrap_or(d.db.connections),
                schema: self.db_schema.unwrap_or(d.db.schema),
            },
            port: self.port.unwrap_or(d.port),
            environment: self.env.unwrap_or(d.environment),
            max_body_size: self.max_body_size.unwrap_or(d.max_body_size),
            api_tokens: self.api_token,
            trace_url: self.trace_url.filter(|url| url.contains("{trace_id}")),
            ca_file: self.ca_file,
            allow_targets: self.allow_targets.unwrap_or(d.allow_targets),
            no_migrate: self.no_migrate,
            roles: self.roles.unwrap_or(d.roles),
            worker: WorkerOptions {
                workers: self.workers.unwrap_or(d.worker.workers).max(1),
                poll_interval: self
                    .interval
                    .map_or(d.worker.poll_interval, Duration::from_millis),
                max_poll_interval: self
                    .max_interval
                    .map_or(d.worker.max_poll_interval, Duration::from_millis),
                prefetch: self.prefetch.unwrap_or(d.worker.prefetch),
                timeout: self.timeout.unwrap_or(d.worker.timeout).min(max_timeout),
                max_timeout,
                max_response_size: self.max_response_size.unwrap_or(d.worker.max_response_size),
                shutdown_timeout: self
                    .shutdown_timeout
                    .map_or(d.worker.shutdown_timeout, Duration::from_secs),
                signing_keys: self.signing_key,
            },
            scheduler: SchedulerOptions {
                poll_interval: d.scheduler.poll_interval,
                prefetch: d.scheduler.prefetch,
                retention_hours: self.retention_hours.unwrap_or(d.scheduler.retention_hours),
            },
        };
        options.validate()?;
        Ok(options)
    }
}

impl Run {
    /// Fills the options not given on the command line from `var("IRISQO_<OPTION>")`.
    pub fn fill_from(&mut self, var: impl Fn(&str) -> Option<String>) -> Result<(), String> {
        take(&mut self.db_url, "IRISQO_DB_URL", &var)?;
        take(&mut self.db_connections, "IRISQO_DB_CONNECTIONS", &var)?;
        take(&mut self.db_schema, "IRISQO_DB_SCHEMA", &var)?;
        take(&mut self.env, "IRISQO_ENV", &var)?;
        Ok(())
    }

    /// The flags over the defaults of [`Options::new`], checked; `--recreate` is refused in
    /// production and for `public`.
    pub fn into_command(self) -> Result<Command, String> {
        let url = self.db_url.ok_or("--db-url or IRISQO_DB_URL must be set")?;
        let mut options = Options::new(url);
        options.db.connections = self.db_connections.unwrap_or(options.db.connections);
        options.db.schema = self.db_schema.unwrap_or(options.db.schema);
        options.environment = self.env.unwrap_or(options.environment);
        options.validate()?;
        if self.recreate {
            if options.environment == Environment::Production {
                return Err(format!(
                    "--recreate drops the schema {:?} with all its data and is refused in production \
                     (the default environment unless --env or IRISQO_ENV says otherwise): pass --env development or staging",
                    options.db.schema
                ));
            }
            // `public` belongs to the database, it is not a schema of ours
            if options.db.schema == "public" {
                return Err("--recreate does not drop the schema \"public\"".to_owned());
            }
        }
        Ok(Command::MigrationsRun {
            options,
            recreate: self.recreate,
        })
    }
}

impl Sql {
    /// The schema: the flag, `IRISQO_DB_SCHEMA`, then the default; checked, as it is quoted into SQL.
    pub fn into_command(mut self, var: impl Fn(&str) -> Option<String>) -> Result<Command, String> {
        take(&mut self.db_schema, "IRISQO_DB_SCHEMA", &var)?;
        let schema = self.db_schema.unwrap_or_else(|| DEFAULT_SCHEMA.to_owned());
        check_schema(&schema)?;
        Ok(Command::MigrationsSql { schema })
    }
}

/// Reads the `IRISQO_<OPTION>` variable of an option the command line does not give.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn serve(args: &[&str]) -> xflags::Result<Serve> {
        let args = args.iter().map(Into::into).collect();
        match Irisqo::from_vec(args)?.subcommand {
            IrisqoCmd::Serve(serve) => Ok(serve),
            IrisqoCmd::Migrations(_) => panic!("not the server command"),
        }
    }

    #[test]
    fn options_fall_back_to_the_environment() {
        let vars = |name: &str| match name {
            "IRISQO_PORT" => Some("9000".to_owned()),
            "IRISQO_MAX_BODY_SIZE" => Some("131072".to_owned()),
            "IRISQO_MAX_RESPONSE_SIZE" => Some("10485760".to_owned()),
            "IRISQO_MAX_TIMEOUT" => Some("1000".to_owned()),
            "IRISQO_ENV" => Some("dev".to_owned()),
            _ => None,
        };
        let mut serve = serve(&["--port", "8000"]).expect("options");
        serve.fill_from(vars).expect("valid");
        assert_eq!(serve.port, Some(8000), "the command line wins");
        assert_eq!(serve.max_body_size, Some(131072));
        assert_eq!(serve.max_response_size, Some(10_485_760));
        assert_eq!(serve.max_timeout, Some(1000));
        assert_eq!(serve.env, Some(Environment::Development));
        assert_eq!(serve.workers, None);

        let mut bad = self::serve(&[]).expect("options");
        let err = bad
            .fill_from(|name| (name == "IRISQO_PORT").then(|| "port".to_owned()))
            .expect_err("not a number");
        assert!(err.starts_with("IRISQO_PORT: "), "{err}");
    }

    #[test]
    fn a_bad_token_is_not_echoed() {
        let secret = "s3cr3t-short";
        let err = serve(&[])
            .expect("options")
            .fill_from(|name| (name == "IRISQO_API_TOKEN").then(|| secret.to_owned()))
            .expect_err("too short");
        assert!(
            err.starts_with("IRISQO_API_TOKEN:") && !err.contains(secret),
            "{err}"
        );

        let err = serve(&["--api-token", secret]).expect_err("too short");
        assert!(!err.to_string().contains(secret), "{err}");
    }

    #[test]
    fn flags_become_the_options_of_the_library() {
        let err = serve(&[])
            .expect("flags")
            .into_options()
            .expect_err("no db");
        assert!(err.to_string().contains("--db-url"), "{err}");

        let defaults = serve(&["--db-url", "postgres://x"])
            .expect("flags")
            .into_options()
            .expect("options");
        let expected = Options::new("postgres://x");
        assert_eq!(defaults.port, expected.port);
        assert_eq!(defaults.worker.workers, expected.worker.workers);
        assert_eq!(defaults.worker.timeout, expected.worker.timeout);
        assert_eq!(
            defaults.scheduler.retention_hours,
            expected.scheduler.retention_hours
        );

        // forgiving: lowered, raised, ignored
        let options = serve(&[
            "--db-url",
            "postgres://x",
            "--max-timeout",
            "1000",
            "--workers",
            "0",
            "--trace-url",
            "http://t/",
            "--no-migrate",
        ])
        .expect("flags")
        .into_options()
        .expect("options");
        assert_eq!(options.worker.timeout, 1000);
        assert_eq!(options.worker.workers, 1);
        assert_eq!(options.trace_url, None);
        assert!(options.no_migrate);

        let roles = |args: &[&str], var: Option<&str>| {
            let args = [&["--db-url", "postgres://x"], args].concat();
            serve(&args)
                .map_err(|err| err.to_string())
                .and_then(|mut serve| {
                    serve
                        .fill_from(|name| var.filter(|_| name == "IRISQO_ROLES").map(str::to_owned))
                        .and_then(|()| serve.into_options())
                })
                .map(|options| options.roles.to_string())
        };
        assert_eq!(
            roles(&[], None),
            Ok("api,ui,worker,scheduler,retention".to_owned())
        );
        assert_eq!(
            roles(&["--roles", "worker"], Some("api")),
            Ok("worker".to_owned())
        );
        assert_eq!(roles(&[], Some("ui, worker")), Ok("ui,worker".to_owned()));
        assert!(
            roles(&["--roles", "http"], None).is_err(),
            "http is two roles now"
        );
        let err =
            roles(&["--roles", "retention", "--retention-hours", "0"], None).expect_err("none");
        assert!(err.contains("no role"), "{err}");

        let err = serve(&["--db-url", "postgres://x", "--db-schema", "Bad"])
            .expect("flags")
            .into_options()
            .expect_err("schema");
        assert!(err.to_string().contains("schema"), "{err}");
    }

    #[test]
    fn recreate_is_refused_in_production_and_for_public() {
        // `irisqo migrations run --db-url postgres://x <args>` with `IRISQO_ENV` = `env`
        let migrate = |args: &[&str], env: Option<&str>| {
            let args = ["migrations", "run", "--db-url", "postgres://x"]
                .iter()
                .chain(args)
                .map(Into::into)
                .collect();
            let IrisqoCmd::Migrations(Migrations {
                subcommand: MigrationsCmd::Run(mut migrate),
            }) = Irisqo::from_vec(args).expect("args").subcommand
            else {
                unreachable!()
            };
            migrate
                .fill_from(|name| env.filter(|_| name == "IRISQO_ENV").map(str::to_owned))
                .and_then(|()| migrate.into_command())
                .map(|_| ())
        };
        let err = migrate(&["--recreate"], None).expect_err("production is the default");
        assert!(err.contains("refused in production"), "{err}");
        assert!(migrate(&["--recreate", "--env", "prod"], None).is_err());
        assert!(migrate(&["--recreate"], Some("production")).is_err());
        assert!(migrate(&["--recreate", "--env", "dev"], None).is_ok());
        assert!(migrate(&["--recreate"], Some("staging")).is_ok());
        assert!(migrate(&[], None).is_ok(), "no --recreate: any environment");
        let public = ["--recreate", "--env", "dev", "--db-schema", "public"];
        let err = migrate(&public, None).expect_err("public");
        assert!(err.contains("public"), "{err}");
    }

    #[test]
    fn migrations_sql_needs_no_database_and_checks_the_schema() {
        let sql = |args: &[&str], var: Option<&str>| {
            let args = ["migrations", "sql"]
                .iter()
                .chain(args)
                .map(Into::into)
                .collect();
            let IrisqoCmd::Migrations(Migrations {
                subcommand: MigrationsCmd::Sql(sql),
            }) = Irisqo::from_vec(args).expect("args").subcommand
            else {
                unreachable!()
            };
            sql.into_command(|name| {
                var.filter(|_| name == "IRISQO_DB_SCHEMA")
                    .map(str::to_owned)
            })
            .map(|command| match command {
                Command::MigrationsSql { schema } => schema,
                Command::Serve(_) | Command::MigrationsRun { .. } => unreachable!(),
            })
        };
        assert_eq!(sql(&[], None), Ok("irisqo".to_owned()));
        assert_eq!(
            sql(&["--db-schema", "mine"], Some("env")),
            Ok("mine".to_owned())
        );
        assert_eq!(sql(&[], Some("env")), Ok("env".to_owned()));
        assert!(sql(&["--db-schema", "Bad"], None).is_err());
    }
}
