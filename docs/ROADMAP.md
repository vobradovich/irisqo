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
  same job description.
- **Safe defaults.** Production unless told otherwise, private targets refused unless allowed,
  credentials of the target never given back.
- **Postgres only** for the queue. Other databases, if ever, only for the relay outbox.

## Where we are

- Queue in Postgres with `FOR UPDATE SKIP LOCKED`; several instances, heartbeats, a lock reaper,
  graceful shutdown that releases the jobs it cannot finish.
- Jobs now, delayed or at a time; interval and cron schedules with an end; retry policies
  (immediate, fixed, Fibonacci) with jitter and `Retry-After`; timeouts; idempotency by external id.
- `/to/{url}?_params` API, RFC 9457 errors, 404s; bearer tokens for the API, UI login, CSRF
  protection; refused private targets (SSRF) with an allowlist; forwarded header values never given
  back.
- OpenTelemetry: one trace per job across its attempts, metrics and logs over OTLP.
- UI: jobs, job, new job, schedules, instances; light and dark themes.
- `IRISQO_*` variables for every option, `--env development|staging|production`.

## Phase 1: Foundations

The schema and the job format first: everything else builds on them, and they are cheapest to change
now, before anyone depends on them.

1. **A configurable schema.** `--db-schema` (`IRISQO_DB_SCHEMA`), `irisqo` by default, set as the
   `search_path` of every connection, so the migrations table lives there too. A migration moves the
   existing tables out of `public`. The tests run in a schema other than the default one, so a
   hard-coded name cannot slip in.
2. **The job format v1 and the JSON API.** `POST /api/v1/jobs` with the whole job in JSON (target,
   method, headers, body, when, retry, timeout, external id; queue and callback from phase 2), and
   `POST /api/v1/jobs/batch`: an array in one transaction, the invalid items reported with RFC 9457
   pointers (`/3/url`). `/to/` stays for the simple cases. An OpenAPI description of the API.
3. **Run modes.** `irisqo serve` (API, UI, workers, as now) and `irisqo worker` (workers, probes and
   metrics; `--ui` adds the UI for the queue of that database). `--no-migrate`, and
   `irisqo migrations sql` printing the migrations for Flyway, EF or Liquibase.
4. **Transactional enqueue.** `<schema>.enqueue_v1(job jsonb) RETURNS bigint`: the job format v1,
   called by the service in its own transaction; simple checks raise clear exceptions, the rest is
   checked by the worker (an invalid job fails with the reason). The version in the name lets the
   contract change without breaking callers. It sends `pg_notify` on commit, and the workers `LISTEN`,
   so a job starts at once; polling stays as a fallback.
5. **Gentle on a shared database.** The response body is stored only with `--store-response` in the
   worker mode, a short retention by default, a small pool of its own.
6. **Deployment.** A Dockerfile (on `rustls`, no OpenSSL), a Helm chart with both modes (`serve`, and
   `worker` as a sidecar), `IRISQO_ALLOW_TARGETS` with the pod and service CIDRs of the cluster (the
   targets of an internal cluster are private addresses), `rust-version`, image releases in CI.

## Phase 2: Flow control and callbacks

Required: without ordering an outbox loses its point, and without callbacks a caller learns nothing.

1. **Flow keys.** A job may name a `queue`:
   - a declared queue has a parallelism N, an optional rate per second, and can be paused;
   - any other name is a key of its own, N = 1 by default: `queue: "order-42"` runs the jobs of an
     order one by one, in order, while other orders run in parallel;
   - the target host is an implicit key (`--host-parallelism`), so a job runs only when both its keys
     have room: a protection for internal services;
   - a job held back by a limit stays queued and holds no worker;
   - with N = 1 a retrying job holds its queue (strict order), unless the queue says otherwise.

   A Queues page in the UI (depth, running, paused, pause and resume), metrics per queue.
2. **Callbacks.** Both are jobs themselves (`parent_id`): retried, traced, shown in the UI.
   - `callback`: on the final outcome (completed, failed after the retries, cancelled), a POST of an
     envelope: job id, status, attempts, the response (status, headers, body) or the error;
     `callback_attempts: all` for every attempt.
   - `then`: on a 2xx, the response body as is to the next URL (`Irisqo-Parent-Id`): a continuation,
     a chain A → B → C.
3. **Signed deliveries.** `Irisqo-Signature`, an HMAC of the timestamp and the body with
   `--signing-key` (several keys for a rotation), on every delivery and callback, so a receiver knows
   the call comes from irisqo.
4. **Schedules a service declares.** `PUT /api/v1/schedules/{name}` creates or updates (a service
   declares its schedules at start, as `RecurringJob.AddOrUpdate`), cron in a time zone with daylight
   saving time, run now, and a policy for the runs missed while down.

## Phase 3: Transports and SDKs

1. **Kafka.** A job protocol next to HTTP (`topic`, `key`, headers), behind the `kafka` feature; the
   message key is the flow key by default, so the order holds in the partition. The outbox to Kafka
   without Debezium.
2. **.NET SDK** first, the Hangfire replacement:
   - a client: enqueue, schedule, declare schedules, cancel, batch, over the API or, embedded, through
     `enqueue_v1` in the current EF Core / Npgsql transaction;
   - a receiver for ASP.NET Core: checks the signature, drops repeated `Irisqo-Job-Id`s, routes a job
     to its handler (`/irisqo/jobs/{name}`): HTTP cannot carry a lambda the way Hangfire does, so the
     SDK maps job names to handlers in the same service.
3. **Go, Java (Spring), Python (FastAPI) SDKs**: clients from the OpenAPI description, `enqueue_v1` in
   the transaction of the language (pgx, JDBC with `@Transactional`, psycopg / SQLAlchemy), receivers.

## Phase 4: Operations

- Modify and retry: a new job from a failed or cancelled one with changes, `retry_of` the original
  (kept for the audit); hidden header values are kept unless replaced.
- Bulk actions in the UI and the API: retry, cancel, delete by filter.
- Batches with a callback when all their jobs are done; priorities of queues.
- Failed jobs as a dead letter queue: metrics and alerts.
- Release the jobs of a stuck instance from the Instances page.
- UI: the stats above the filters, field borders at 3:1 contrast, screenshots that follow the
  reader's theme in the README.

## Phase 5: Scale

- The jobs list queried from the table of the state, with `processed (status, id)` and a `pg_trgm`
  index on the URL.
- One scheduler and one retention runner across instances (advisory lock).
- Retention by dropping partitions (`jobs`, `processed`, `history` by range of id).
- `sqlx::query!` with offline data, checked at build time.
- Load tests: throughput and latency against the number of instances and workers.
- Namespaces: tokens scoped to a project, quotas, when several teams share a central irisqo.
- Search: full text with ParadeDB `pg_search` in the same database; history and analytics in
  Quickwit or OpenObserve, fed from `history` (already written in the transaction of each change).

## Later

- **Relay** (`irisqo relay`): an outbox table in a service database (any database, SQL Server
  included), copied to a central irisqo in batches, idempotently, and deleted; or straight to Kafka.
  The body is written twice, but the central features (one UI, limits across services) stay.
- **A Rust library**: `irisqo-core` (models, database, runner) apart from the server, for enqueueing
  and running in a Rust process.
- A CHANGELOG and tagged releases.

## Order and dependencies

- The schema (1.1) comes before anything that names a table, `enqueue_v1` included.
- The job format (1.2) comes before `enqueue_v1`, the batch API and the SDKs: they all take it.
- Flow keys (2.1) come before Kafka (3.1): the order of an outbox is the order of its keys.
- Signed deliveries (2.3) come before callbacks reach other services and before the SDK receivers.

## Open decisions

- `enqueue_v1(jsonb)` or typed parameters: `jsonb` keeps one format.
- Who migrates an embedded schema by default: the worker (with `--no-migrate` to opt out).
- Whether a retrying job holds a queue of N = 1 (strict order) or lets the next one go.
- Payload of a callback: the body as base64 or as text when it is text.
