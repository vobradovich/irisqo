# Roadmap

## Goal

irisqo is a job queue for the services of an internal Kubernetes cluster or a cloud, in place of
Hangfire (.NET) and its counterparts in Java, Python and Go. A service hands it a call to make now,
later or on a schedule; irisqo makes it, retries it, and reports the outcome. Calls are HTTP, and
Kafka later; SDKs make it native to each language.

Two ways to run it, on one database schema and one binary:

- **Embedded** (the main one): the `irisqo` schema lives in the database of the service. The service
  enqueues a job in its own transaction (a transactional outbox: the job exists if and only if the
  business data is committed), and irisqo workers run next to it, as a sidecar in its pod or a
  deployment of their own, on that database.
- **Central**: irisqo with a database of its own, called over its API by many services. This is the
  embedded mode where the "service" is irisqo itself: same schema, same workers, same UI.

A relay from a service database to a central irisqo (an outbox table copied over, the body written
twice) is an option for later, when one shared irisqo is wanted: see [Later](#later).

### Principles

- **Postgres is the source of truth.** The state of a job is the table it is in; anything else (a
  search index, metrics) is derived and can be rebuilt.
- **At least once.** Every delivery may repeat: it carries `Irisqo-Job-Id` to drop duplicates.
- **One job format.** The JSON API, the SQL enqueue function, the batch API and the SDKs take the
  same job description ([json-v1.md](json-v1.md)).
- **Safe defaults.** Production unless told otherwise, private targets refused unless allowed,
  credentials of the target never given back.
- **Postgres only** for the queue. Other databases, if ever, only for the relay outbox.

Legend: ✅ done, 🔶 partly, ⬜ to do.

## Where we are

Phases 1 and 2 of the first plan are mostly done: the schema, the job format, the transactional
enqueue, flows and callbacks are in `main`. What is left of them is what makes irisqo deployable as an
embedded service (run modes, a notification on enqueue, an image, a chart) and a few edges (a limit per
target host, signed deliveries, a manual run of a schedule).

- ✅ Queue in Postgres with `FOR UPDATE SKIP LOCKED`; several instances, heartbeats, a lock reaper,
  graceful shutdown that releases the jobs it cannot finish.
- ✅ Jobs now, delayed or at a time; priorities; retry policies (fixed, Fibonacci, exponential) with
  jitter and `Retry-After`; timeouts; idempotency by `key`; `--max-response-size`.
- ✅ Schedules: interval and cron, time zones, `until`, a miss policy; declared by name
  (`PUT /api/v1/schedules/{id}`, or a job with `cron` / `interval`).
- ✅ A configurable schema (`--db-schema`, `irisqo` by default; created safely when several instances
  start on a new database).
- ✅ The job format v1 and the JSON API (`POST /api/v1/jobs`), OpenAPI at `/api/v1/openapi.json`,
  `/to/{url}?_params` for the simple cases.
- ✅ `enqueue_v1(job jsonb)`: transactional enqueue from the service ([sql.md](sql.md)).
- ✅ Flows (the flow control): a key per job (`flow_key`), a concurrency per key, a rate (GCRA), pause,
  undeclared keys default to 1; no order guarantee; `PUT /api/v1/flows/{id}`, a Flows page.
- ✅ Callbacks ([callbacks.md](callbacks.md)): `webhook` (signed JSON, Standard Webhooks) and `relay`
  (the response of the job as it is), both child jobs (`parent_id`) with their own retries, history and
  trace; signing keys with a rotation.
- ✅ Security: bearer tokens for the API, UI login, CSRF protection, refused private targets (SSRF)
  with an allowlist, forwarded header values never given back, `problem+json` errors.
- ✅ OpenTelemetry: one trace per job across its attempts, metrics and logs over OTLP.
- ✅ UI: jobs, job, new job, schedules, flows, instances; light and dark themes.
- ✅ `IRISQO_*` variables for every option, `--env development|staging|production`.
- 🔶 Performance work is under way: batched claims, per-state partial indexes, a flow wakes the loader
  when a place frees. A one-table variant (`queue` with a `state` column) is benchmarked against the
  three tables (`scheduled`, `enqueued`, `claimed`); the layout is not final.

## Phase 1: Embedded service (next)

What is left to run irisqo as a sidecar of a service, on the service database.

1. 🔶 **Run modes.** The `instances.roles` column already knows `http`, `worker`, `scheduler`,
   `retention` and `relay`, but one process runs them all.
   - ⬜ Choose the roles of a process: `irisqo serve` (all, as now) and `irisqo worker` (workers, probes
     and metrics; `--ui` adds the UI for the queue of that database).
   - ⬜ `--no-migrate`, and `irisqo migrations sql` printing the migrations for Flyway, EF or
     Liquibase.
2. ⬜ **A notification on enqueue.** `enqueue_v1` sends `pg_notify` on commit and the workers `LISTEN`,
   so a job starts at once; polling stays as a fallback.
3. ⬜ **Gentle on a shared database.** `--store-response` (the response body is kept only with it; the
   option is described in [sql.md](sql.md) but not implemented), a short retention by default in the
   worker mode, a small pool of its own.
4. ⬜ **Deployment.** A Dockerfile (on `rustls`, no OpenSSL), a Helm chart with both modes (`serve`, and
   `worker` as a sidecar), `IRISQO_ALLOW_TARGETS` with the pod and service CIDRs of the cluster (the
   targets of an internal cluster are private addresses), `rust-version`, image releases in CI.
5. ⬜ **Docs that match the code.** README "Planned" lists callbacks as to do; the roadmap and the
   README must say the same.

## Phase 2: Flow control and callbacks, the edges

The core is in. What is left:

1. ⬜ **A limit per target host.** An implicit flow key `host:<host>` (`--host-parallelism`), so a job
   runs only when both its flow and its host have room: a protection for the internal services it
   calls. Per-host metrics.
2. ⬜ **Signed deliveries.** Callbacks are signed; ordinary deliveries are not. `Irisqo-Signature`
   (an HMAC of the timestamp and the body with `--signing-key`) on every delivery, so a receiver
   knows the call comes from irisqo.
3. 🔶 **Continuations.** A `relay` callback passes the response on to the next URL, one step. Chains
   (A → B → C) and a link from a retry to its original (`retry_of`) are not there.
4. ⬜ **Schedules: run now.** Start a schedule's job at once; the miss policy is done.
5. ⬜ **Ordering.** Flows give no order today, by design. If outbox use needs order by aggregate, it
   comes back as an option of a flow (`ordered`, concurrency 1), decided with the benchmark results.

## Phase 3: Batches, transports and SDKs

1. ⬜ **Batches.** `POST /api/v1/jobs/batch`: an array in one transaction, the invalid items reported
   with RFC 9457 pointers (`/3/url`); then `batch_id` and a callback when all the jobs are done.
2. ⬜ **Kafka.** A job protocol next to HTTP (`topic`, `key`, headers), behind a `kafka` feature; the
   message key is the flow key by default. The outbox to Kafka without Debezium. (The place is marked in
   the code, `// Kafka,`, nothing more.)
3. ⬜ **.NET SDK** first, the Hangfire replacement:
   - a client: enqueue, schedule, declare schedules, cancel, batch, over the API or, embedded, through
     `enqueue_v1` in the current EF Core / Npgsql transaction;
   - a receiver for ASP.NET Core: checks the signature, drops repeated `Irisqo-Job-Id`s, routes a job
     to its handler (`/irisqo/jobs/{name}`): HTTP cannot carry a lambda the way Hangfire does, so the
     SDK maps job names to handlers in the same service.
4. ⬜ **Go, Java (Spring), Python (FastAPI) SDKs**: clients from the OpenAPI description, `enqueue_v1`
   in the transaction of the language (pgx, JDBC with `@Transactional`, psycopg / SQLAlchemy),
   receivers.

## Phase 4: Operations

- ⬜ Modify and retry: a new job from a failed or cancelled one with changes, `retry_of` the original
  (kept for the audit); hidden header values are kept unless replaced. A plain retry exists.
- ⬜ Bulk actions in the UI and the API: retry, cancel, delete by filter.
- ⬜ Failed jobs as a dead letter queue: metrics and alerts.
- ⬜ Release the jobs of a stuck instance from the Instances page.
- ⬜ UI: the stats above the filters, field borders at 3:1 contrast, screenshots that follow the
  reader's theme in the README.

## Phase 5: Scale

- 🔶 Load tests: throughput and latency against the number of instances and workers (the benchmark
  branches are the start).
- 🔶 The jobs list queried from the table of the state: a `processed (status, id)` index is there; a
  `pg_trgm` index on the URL is not.
- ⬜ One scheduler and one retention runner across instances (advisory lock).
- ⬜ Retention by dropping partitions (`jobs`, `processed`, `history` by range of id).
- ⬜ `sqlx::query!` with offline data, checked at build time.
- ⬜ Namespaces: tokens scoped to a project, quotas, when several teams share a central irisqo.
- ⬜ Search: full text with ParadeDB `pg_search` in the same database; history and analytics in
  Quickwit or OpenObserve, fed from `history` (its `seq` is the cursor; events are written in the
  transaction of each change).

## Later

- **Relay** (`irisqo relay`): an outbox table in a service database (any database, SQL Server
  included), copied to a central irisqo in batches, idempotently, and deleted; or straight to Kafka.
  The body is written twice, but the central features (one UI, limits across services) stay. The
  `relay` role is already in the `instances.roles` check.
- **A Rust library**: `irisqo-core` (models, database, runner) apart from the server, for enqueueing
  and running in a Rust process.
- A CHANGELOG and tagged releases.

## Order and dependencies

- Run modes (1.1) and the notification (1.2) come before the chart (1.4): the chart deploys them.
- The host limit (2.1) reuses the flow machinery: it is a key the server adds, not a new mechanism.
- Signed deliveries (2.2) come before the SDK receivers (3.3), which verify them.
- Batches (3.1) come before the SDK clients that expose them.
- The table layout is settled by the benchmark before any change that touches the claim query
  (host limit, ordering).

## Open decisions

- The queue tables: three (`scheduled`, `enqueued`, `claimed`) or one with a `state` column.
- Whether ordering by aggregate returns as an option of a flow, or stays out.
- Who migrates an embedded schema by default: the worker (with `--no-migrate` to opt out).
- Payload of a batch callback: the outcomes of all the jobs, or only the counts.
