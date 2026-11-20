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
enqueue, flows, callbacks and the run modes are in `main`. What is left of them is what makes irisqo
deployable as an embedded service (an image, a chart) and a few edges (a
limit per target host, signed deliveries, a manual run of a schedule).

Items that hold several unrelated tasks are split into numbered steps (`1.4.1`, `1.4.2`, ...); each
step is meant to be one pull request, and says what it needs.

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
- ✅ Run modes: `--roles` (`api`, `ui`, `worker`, `scheduler`, `retention`), `--no-migrate`,
  `irisqo migrations run` and `irisqo migrations sql` (for Flyway, EF or Liquibase).
- 🔶 Performance work is under way: batched claims, per-state partial indexes, a flow wakes the loader
  when a place frees. A one-table variant (`queue` with a `state` column) is benchmarked against the
  three tables (`scheduled`, `enqueued`, `claimed`); the layout is not final.

## Phase 1: Embedded service (next)

What is left to run irisqo as a sidecar of a service, on the service database.

1. ✅ **Run modes.** `--roles`, `--no-migrate`, `irisqo migrations run | sql`.

2. ✅ **A job starts soon after it is enqueued: by polling, not by `pg_notify`.** An idle worker
   polls every 200 ms, the pause doubling up to 1 s while the queue stays empty; a freed flow place
   wakes the loader at once. The scheduler moves due jobs every second.
   - Why not `pg_notify`: a `NOTIFY` in the transaction that inserts a job serializes the commits
     (the notification queue is locked until the commit ends); measured, 16 clients insert 3 to 8
     times fewer jobs per second, with any number of listeners. It would also leave out the main
     embedded case: a service calls `enqueue_v1` in its own transaction, and no irisqo process is
     there to notify anyone afterwards. Polling serves every way a job gets in, and works through
     any pooler.
   - So `enqueue_v1` and `promote_v1` send no notification.
   - Later, only if the polling cost shows in a measurement: a notification from the API path and
     the scheduler, outside the transaction that inserts, coalesced to one in about 50 ms per
     instance, with a `LISTEN` connection of its own (it counts in `max_connections`, and needs a
     session pooler, see 1.4.9), polling kept as the fallback and a poll after a reconnect. The
     channel is named from `current_schema()`, so a schema name must stay within 54 characters
     (`check_schema` lets 63 through today), and the payload is the same everywhere.

3. ⬜ **Gentle on a shared database.** Three separate changes, none depends on the others.
   1. `--store-response`: the response body is kept only with it. Described in [sql.md](sql.md), not
      implemented; the default is not to keep it in the worker-only mode.
   2. A shorter retention for a sidecar on a service database (`--retention-hours` is 168 for every
      process today): a default for a process with the `worker` role and no `api`, and documented
      with the cost of keeping history in the database of the service.
   3. A pool of its own per role: `--db-connections` today is one number for the process; the
      sidecar needs a small one, the API a larger one.

4. ⬜ **Deployment.** An image, a chart and the pipeline that publishes them. Steps, in an order that
   keeps each one shippable alone:
   1. **TLS on `rustls`, no OpenSSL.** OpenSSL comes from two places only: `hyper-tls` (calls to
      targets) and `sqlx` with `tls-native-tls` (Postgres); the OTLP export is plain HTTP and
      does not need it. Swap them for `hyper-rustls` and sqlx's rustls feature.
      - Decide the trust roots: the webpki set only breaks a private CA, which an internal cluster
        usually has. Read the system store (`rustls-native-certs`) and honour `SSL_CERT_FILE` /
        a `--ca-file`.
      - Postgres `sslmode=require | verify-full` against a container in the tests.
      - Done when `cargo tree -i openssl-sys` is empty.
   2. **`rust-version`.** Find the minimum (edition 2024 needs 1.85 at least), set it in
      `Cargo.toml`, and add a CI job on that exact toolchain next to `stable`.
   3. **Dockerfile.** Multi-stage, with a dependency cache layer; the assets and the migrations are
      compiled into the binary, so the final stage holds the binary alone (a distroless or scratch
      base; a static build is possible once 1.4.1 is done). Runs as non-root, read-only root file
      system, `--help` as a smoke test in CI, `.dockerignore`, the version as a build argument.
   4. **A compose example of the embedded mode.** A service, its Postgres and an `irisqo --roles
      worker` sidecar on the same database, enqueueing with `enqueue_v1` in a transaction: an
      end-to-end proof before the chart, and the quick start in the README.
   5. **Image release in CI.** Build and push to GHCR on a tag, `amd64` and `arm64`, tags by version
      and commit, provenance and an SBOM. Needs 1.4.3 and the first tagged release (see Later).
   6. **Helm chart, shape of the workloads.** Three ways in `values.yaml`, one chart:
      - *all in one* (`serve`, all roles, a Deployment and an HPA): the central irisqo;
      - *split*: the API and the UI, the workers, and the scheduler with the retention (one or two
        replicas) as separate Deployments, scaled apart;
      - *sidecar*: not a chart of its own but a documented container spec (`--roles worker`) to
        paste into the chart of a service.
   7. **Helm chart, what a pod needs.** Probes on `/live` and `/ready`;
      `terminationGracePeriodSeconds` above `--shutdown-timeout` (20 s now, 30 s in Kubernetes);
      a PodDisruptionBudget, resources, a restricted `securityContext`; the DB URL, the API token
      and the signing keys from a Secret (`existingSecret`); the OTLP variables; migrations as a
      pre-install / pre-upgrade Job (`irisqo migrations run`) with `--no-migrate` on the pods.
   8. **Targets of the cluster.** The targets are private addresses, so the chart sets
      `IRISQO_ALLOW_TARGETS` from values (the pod and service CIDRs, how to find them in the
      docs) and keeps `--env production`. The cloud metadata address stays refused; a test in the
      chart's CI renders the variable.
   9. **PgBouncer and the schema.** The schema is the connection's `search_path`, sent as a startup
      parameter. A pooler in transaction mode refuses or drops it, and then every query misses its
      tables. Check against PgBouncer (and the pooler of the main managed Postgres services); if
      it fails, qualify the schema in the queries or set it on the role (`ALTER ROLE ... SET
      search_path`), and document which poolers work in which mode (polling works in any mode).
   10. **Chart tests.** `helm lint` and `helm template` on every values example; an end-to-end run on
      a throwaway cluster (kind): install the chart and Postgres, enqueue a call to an echo
      service, see it complete.
   11. **Deployment docs** (`docs/deploy.md`): central or embedded; the pool arithmetic
      (`--db-connections` × replicas against Postgres `max_connections`); the poolers; upgrades
      (migrations first, then pods); what to alert on.

5. ⬜ **Docs that match the code.**
   1. The README "Planned" list: callbacks and the run modes are done; the list and this roadmap
      must say the same.
   2. A docs index in the README (json-v1, sql, callbacks, problems, deploy).
   3. A short "Embedded mode" guide: the schema, `enqueue_v1` in a transaction, a sidecar worker.

## Phase 2: Flow control and callbacks, the edges

The core is in. What is left:

1. ⬜ **A limit per target host.** A protection for the internal services irisqo calls.
   1. An implicit flow key `host:<host>`: a job runs only when both its own flow and its host have
      room. Today `host:…` is only a name used as an example; the claim has one key per job, so
      this first needs the claim to take a second key (decide with the table benchmark).
   2. `--host-parallelism` for the default, and `PUT /api/v1/flows/host:<host>` to set one host.
   3. Per-host metrics, and the hosts on the Flows page.
2. ⬜ **Signed deliveries.** Callbacks are signed; ordinary deliveries are not. `Irisqo-Signature`
   (an HMAC of the timestamp and the body with `--signing-key`) on every delivery, so a receiver
   knows the call comes from irisqo.
3. 🔶 **Continuations.** A `relay` callback passes the response on to the next URL, one step. Chains
   (A → B → C) and a link from a retry to its original (`retry_of`) are not there.
4. ⬜ **Schedules: run now.** Start a schedule's job at once; the miss policy is done.
5. ⬜ **Ordering.** Flows give no order today, by design. If outbox use needs order by aggregate, it
   comes back as an option of a flow (`ordered`, concurrency 1), decided with the benchmark results.

## Phase 3: Batches, transports and SDKs

1. ⬜ **Batches.**
   1. `POST /api/v1/jobs/batch`: an array in one transaction, the invalid items reported with
      RFC 9457 pointers (`/3/url`); the same for `enqueue_v1` (an array, or a set-returning twin).
   2. A `batches` table and `batch_id` on the job, with the counters of completed, failed and
      cancelled.
   3. A callback when all the jobs of a batch are done (the open decision below).
   4. The batch on the job page and a batches list in the UI.
2. ⬜ **Kafka.** A job protocol next to HTTP (`topic`, `key`, headers), behind a `kafka` feature; the
   message key is the flow key by default. The outbox to Kafka without Debezium. (The place is marked in
   the code, `// Kafka,`, nothing more.)
3. ⬜ **.NET SDK** first, the Hangfire replacement. HTTP cannot carry a lambda the way Hangfire does,
   so the SDK maps job names to handlers in the same service.
   1. The client over the API, generated from the OpenAPI description (enqueue, schedule, declare
      schedules, cancel, batch), with a thin hand-written layer on top.
   2. The embedded client: `enqueue_v1` in the current EF Core / Npgsql transaction
      (`db.Irisqo().EnqueueAsync(job)`).
   3. The receiver for ASP.NET Core: checks the signature (needs 2.2), drops repeated
      `Irisqo-Job-Id`s, routes a job to its handler (`/irisqo/jobs/{name}`).
   4. Tests against a real irisqo and Postgres (Testcontainers), including the rollback case.
   5. NuGet packaging, samples, and a "from Hangfire" guide: `BackgroundJob.Enqueue`,
      `Schedule`, `RecurringJob.AddOrUpdate` and continuations side by side with their equivalents.
4. ⬜ **Go, Java (Spring), Python (FastAPI) SDKs**, one at a time, in the order of demand. Each is the
   same four parts: a client from the OpenAPI description, `enqueue_v1` in the transaction of the
   language (pgx, JDBC with `@Transactional`, psycopg / SQLAlchemy), a receiver, a package.

## Phase 4: Operations

Independent items; any order.

1. ⬜ **Modify and retry.** A plain retry exists.
   1. The API: a new job from a failed or cancelled one with changes, `retry_of` the original
      (kept for the audit; needs the link from 2.3).
   2. The UI: the New job form filled from the job.
   3. Hidden header values (`••••`) are kept unless replaced.
2. ⬜ **Bulk actions** by filter: retry, cancel, delete.
   1. The filter as a query the API and the UI share (status, flow, key prefix, time).
   2. `POST /api/v1/jobs/bulk` with a dry run that returns the count.
   3. The UI: select by filter, a confirmation with the count.
3. ⬜ **Failed jobs as a dead letter queue.** A metric of failed jobs by flow and target host, alert
   rules as examples, and a "Failed" view with the reason.
4. ⬜ **Release the jobs of a stuck instance** from the Instances page (the lock reaper does it
   after the lease; this does it now).
5. ⬜ **UI polish**, small and unrelated: the stats above the filters; field borders at 3:1 contrast;
   README screenshots that follow the reader's theme.

## Phase 5: Scale

Independent items; the first three are one theme, the rest are not.

1. 🔶 **Load tests.**
   1. A repeatable scenario: jobs per second and the delay from due to started, against the number
      of instances, workers and flows (the benchmark branches are the start).
   2. Results kept in the repository, and a regression threshold in CI for the claim query.
2. ⬜ **The table layout**, from the benchmark: three tables or one with a `state` column.
3. 🔶 **The jobs list from the table of the state.** `processed (status, id)` is there; a `pg_trgm`
   index on the URL is not (the extension may be missing on a service database, so optional).
4. ⬜ **One scheduler and one retention runner across instances** (advisory lock), instead of every
   instance doing the work and the rows deciding who wins.
5. ⬜ **Retention by dropping partitions** (`jobs`, `processed`, `history` by range of id).
6. ⬜ **`sqlx::query!`** with offline data, checked at build time.
7. ⬜ **Namespaces:** tokens scoped to a project, and quotas, when several teams share a central
   irisqo.
8. ⬜ **Search**, two separate ideas:
   1. Full text in the same database with ParadeDB `pg_search`.
   2. History and analytics in Quickwit or OpenObserve, fed from `history` (its `seq` is the
      cursor; events are written in the transaction of each change).

## Later

- **Relay** (`irisqo relay`): an outbox table in a service database (any database, SQL Server
  included), copied to a central irisqo in batches, idempotently, and deleted; or straight to Kafka.
  The body is written twice, but the central features (one UI, limits across services) stay. The
  `relay` role is already in the `instances.roles` check.
- **A Rust library**: `irisqo-core` (models, database, runner) apart from the server, for enqueueing
  and running in a Rust process.
- A CHANGELOG and tagged releases.

## Order and dependencies

- In the deployment (1.4), TLS (1.4.1) and `rust-version` (1.4.2) come before the Dockerfile (1.4.3);
  the Dockerfile before the release (1.4.5) and the chart (1.4.6 and after); the chart's tests
  (1.4.10) last. The compose example (1.4.4) can come right after the Dockerfile.
- The host limit (2.1) reuses the flow machinery but needs a second key in the claim: settle the
  table layout (5.2) first.
- Signed deliveries (2.2) come before the SDK receivers (3.3).
- Batches (3.1) come before the SDK clients that expose them.
- Modify and retry (4.1) needs the `retry_of` link (2.3).

## Open decisions

- The queue tables: three (`scheduled`, `enqueued`, `claimed`) or one with a `state` column.
- Whether ordering by aggregate returns as an option of a flow, or stays out.
- Who migrates an embedded schema by default: the worker (with `--no-migrate` to opt out).
- Payload of a batch callback: the outcomes of all the jobs, or only the counts.
