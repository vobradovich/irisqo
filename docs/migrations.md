# Migrations

The migrations of [`sql/migrations`](../sql/migrations) are embedded in the binary: each version carries
exactly its own. They run with the sqlx migrator, which records what it applied in `_sqlx_migrations`.
[docs/sql.md](sql.md) describes the tables and functions they create.

Everything lives in one Postgres schema, `--db-schema` (`IRISQO_DB_SCHEMA`, `irisqo` by default). The
migrations name no schema: every connection makes it the default one with the startup parameter
`options=-c search_path=…`. The name is up to 63 characters of `a-z`, `0-9` and `_`, starting with a
letter or `_`, not `pg_*`. Several installations can share a database, each in its own schema.

## When they run

**At the start of every instance**, by default: enough for a single instance and for development.

**As a deploy step**, when several instances start together, when the servers connect through a
`transaction` pooler (see [Connection poolers](#connection-poolers)), or when their role may not change
the schema. In Kubernetes: a Job or a Helm `pre-install,pre-upgrade` hook with the new image.

```bash
IRISQO_DB_URL=postgres://user:pass@host/db irisqo migrations run --db-schema irisqo
IRISQO_DB_URL=postgres://user:pass@pooler/db irisqo --db-schema irisqo --no-migrate
```

`irisqo migrations run` takes:

| Option | Default | |
|---|---|---|
| `--db-url` | — | required; better `IRISQO_DB_URL`, a flag shows in `ps` |
| `--db-connections` | 32 | pool size; the migrations use one connection |
| `--db-schema` | irisqo | the schema to create and migrate |
| `--no-create-schema` | — | the schema exists, see [Privileges](#privileges) |
| `--recreate` | — | **drops the schema with all its data** first, see [Development](#development) |
| `--env` | production | only `--recreate` looks at it |

## How a run works

1. `pg_advisory_lock` on an id derived from the database name: a second run waits here. The lock is per
   database, so installations that share one wait for each other too.
2. `CREATE SCHEMA IF NOT EXISTS`, unless `--no-create-schema`.
3. Creates `_sqlx_migrations` when missing.
4. Fails if a recorded migration differs from the binary's (`… was previously applied but has been
   modified`) or is missing in it (`… was previously applied but is missing in the resolved migrations`).
5. Applies each new migration in its own transaction; one that fails rolls back and stops the run.
6. Releases the lock.

A run with nothing new is cheap and safe to repeat. What a database has:

```sql
SELECT version, description, installed_on, success FROM irisqo._sqlx_migrations ORDER BY version;
```

## Versions and rollback

The migrations only go forward: a new version adds migrations and never changes old ones (until the
first release, `0001_init.sql` is still edited in place).

An older version that starts with migrations on a newer database stops at once (step 4). Start it with
`--no-migrate`: it works if the newer migrations kept the schema compatible, otherwise restore a backup
taken before the upgrade.

## Privileges

The migrations and the servers connect as the same role, which owns the tables and functions; the
migrations grant nothing to other roles.

`CREATE SCHEMA IF NOT EXISTS` needs `CREATE` on the database even when the schema exists. Without it the
run fails with `permission denied for database`: a DBA creates the schema, and the migrations (and
servers without `--no-migrate`) get `--no-create-schema`.

```sql
CREATE SCHEMA irisqo AUTHORIZATION irisqo;
-- or, for a schema that someone else owns:
GRANT USAGE, CREATE ON SCHEMA irisqo TO irisqo;
```

## With another tool

`irisqo migrations sql` prints the migrations as one script for Flyway, Liquibase, EF or `psql`, with no
database needed. It creates the schema (unless `--no-create-schema`), sets `search_path` to it, then
each migration under `-- migration N: name`.

```bash
irisqo migrations sql --db-schema irisqo | psql "$DATABASE_URL" -v ON_ERROR_STOP=1
```

Start the servers with `--no-migrate` then: with an empty `_sqlx_migrations` they would apply everything
again and fail. A database that irisqo has already migrated needs a baseline in the other tool at the
last version in `_sqlx_migrations`.

## Connection poolers

A pooler has to keep the startup parameter `options` (the schema) on every server connection, and a
`transaction` pool has to support the named prepared statements of sqlx (`sqlx_s_1`, …). Checked
locally with Postgres 16 and 18, running the migrations and the jobs:

| Pooler | `session` | `transaction` |
|---|---|---|
| PgBouncer 1.26.1 and later | works | works with `max_prepared_statements` above 0 (200 by default since 1.24) |
| PgBouncer 1.20–1.26.0 | works | update: CVE-2026-107349 (an endless loop on parsing `options`), see below |
| PgBouncer 1.19 and older | works | the connection is refused (`unsupported startup parameter`) |
| PgDog | works | works |
| Odyssey 1.5.3 | works | works with `pool_reserve_prepared_statement yes` |
| Supavisor 2.10.2 | works | works with the tenant flag `named_prepared_statements` (`NAMED_PREPARED_STATEMENTS_ENABLED=true`) |
| pgcat 1.2.0 | works with `ALTER ROLE` below | works with `ALTER ROLE` below and `prepared_statements_cache_size` above 0 |

Older PgBouncer in `transaction` mode, until it is updated:
- 1.26.0 keeps the schema with any Postgres.
- 1.20–1.25 keep it only with Postgres 18 and `track_extra_parameters = search_path`. Without the setting
  they refuse the connection; with it on an older Postgres they lose the schema silently.
- Do not use `ignore_startup_parameters = options` as a workaround: it drops the schema.

pgcat drops the startup parameters, so set the schema on the role:

```sql
ALTER ROLE irisqo SET search_path = irisqo;
```

The poolers of the cloud providers (Supabase, Neon, RDS Proxy, Azure, Cloud SQL) are not checked: check
their pool mode and prepared statements.

### Not through a transaction pool

The migrator's lock is a session lock, and a `transaction` pool moves statements between server
connections:

- PgBouncer, PgDog and Supavisor may send the release to another connection (`you don't own a lock`).
  The lock stays on a pooled connection, and every later run waits, a direct one too, and so does every
  instance started without `--no-migrate`, until the pooler closes that connection (`server_idle_timeout`,
  `server_lifetime`).
- Odyssey releases advisory locks after each transaction: nothing waits, but runs are no longer kept
  apart, and instances that start together fail with `duplicate key value violates unique constraint`.
  Supavisor did that too.

So run `irisqo migrations run` directly or through a `session` pool, and start the servers behind a
`transaction` pool with `--no-migrate`.

To release a lock left behind, list the holders, then terminate those idle for a while (a working run
is idle for a moment between statements, and other applications may hold such locks too):

```sql
SELECT l.pid, a.client_addr, a.application_name, a.state, a.state_change
FROM pg_locks l JOIN pg_stat_activity a USING (pid)
WHERE l.locktype = 'advisory' AND l.granted AND l.objsubid = 1 AND a.datname = current_database();

SELECT pg_terminate_backend(l.pid)
FROM pg_locks l JOIN pg_stat_activity a USING (pid)
WHERE l.locktype = 'advisory' AND l.granted AND l.objsubid = 1 AND a.datname = current_database()
  AND a.state = 'idle' AND a.state_change < now() - interval '5 minutes';
```

`RECONNECT` in the PgBouncer console works too.

## Development

```bash
irisqo migrations run --recreate --env development
```

`--recreate` **drops the schema with all its data** and creates it anew. It is refused in `production`,
the default; the environment comes from `--env` or `IRISQO_ENV`, so it cannot tell a production database
declared as `development`. It never drops `public` and is refused with `--no-create-schema`.

## Troubleshooting

| What you see | Why | What to do |
|---|---|---|
| The start or `migrations run` hangs | another run holds the lock, or a lock stays behind in a `transaction` pool | wait, or [release it](#not-through-a-transaction-pool) |
| `permission denied for database` | the role may not create schemas | [`--no-create-schema`](#privileges) |
| `… has been modified` | migrated by a build with another version of that migration | development: `--recreate`; otherwise the binary of the same release |
| `… is missing in the resolved migrations` | an older version on a newer database | the newer version, or [`--no-migrate`](#versions-and-rollback) |
| `relation "jobs" does not exist` | a pooler dropped `options` | [Connection poolers](#connection-poolers) |
| `prepared statement "sqlx_s_1" already exists` (or `does not exist`) | a `transaction` pool without named prepared statements | turn them on, [Connection poolers](#connection-poolers) |
| `duplicate key value violates unique constraint` when several instances start | migrations through a pooler that drops the lock (Odyssey, Supavisor) | restart them; run the migrations as a deploy step |
