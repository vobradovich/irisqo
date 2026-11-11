# irisqo

Self-hosted **HTTP job queue** on Postgres.

A job is an **HTTP request** (method, URL, headers, body). Services enqueue calls to their own endpoints or to others.
Retries, delays, cron, and concurrency live in **irisqo**, not in an in-process job SDK.

```http
POST https://irisqo/to/https://orders.internal/api/sync
Authorization: Bearer …
Irisqo-Forward-Authorization: Bearer …
```

Like QStash or Zeplo, but on **your** Postgres, calling **your** services.

**Status:** early product (usable PoC). Core delivery works; items under Planned are in progress.  
**License:** MIT.

### Design

- Postgres Backend
- Claim with `FOR UPDATE SKIP LOCKED`
- KISS
- DRY: outbox in Postgres, not reimplemented in every service

### Features

#### Working now

- [x] Persistent queue on Postgres
- [x] Multi-instance scaling (heartbeats, lock reaping, graceful shutdown)
- [x] Retries with backoff, delays, timeouts
- [x] Schedules: interval and cron
- [x] Proxy enqueue via `/to/{url}`, idempotency with `_key`
- [x] Jobs and schedules API (cancel, retry)
- [x] Server-rendered UI (htmx, Pico CSS, no CDN)
- [x] OpenTelemetry (traces, metrics, logs over OTLP)
- [x] SSRF protections, bearer auth, `problem+json` errors

#### Planned

- [ ] Job format v1 (JSON): `POST /api/v1/jobs`, batch, OpenAPI
- [ ] Transactional enqueue: `enqueue_v1(job jsonb)` in the service transaction ([docs/sql.md](docs/sql.md); format not stable yet)
- [ ] Flow keys: per-key parallelism, rate, pause; dynamic keys default to N = 1; per-host limit; Queues UI and metrics
- [ ] Named schedules: upsert `PUT /api/v1/schedules/{name}`, time zones, run now, miss policy; human ids (`bold-lazy-heron`)
- [ ] Callbacks / `then` as child jobs (`parent_id`)
- [ ] Signed deliveries (`Irisqo-Signature`, HMAC, key rotation)
- [ ] Run modes: `irisqo serve` / `irisqo worker`; Docker image, Helm chart
- [ ] Batches, bulk actions, richer search, optional partitioning for large histories


## API

### Queue
To queue a request, simply prefix a request with {{host}}/to/ and we’ll queue and forward the request with the exact same method/body/query.
```
@host=http://localhost:8102

###
GET {{host}}/to/https://postman-echo.com/get
```

Response Headers
- `irisqo-job-id` - this is the ID of the request job
- `irisqo-schedule-id` - ID of the new schedule
- `irisqo-key` - the key of the job: `_key` as given, or `{schedule-id}-{unix secs}` for a schedule run (with `_key`, the schedule id is `_key`).

### Headers
The target gets only the body headers (`Content-Type`, `Content-Encoding`, `Content-Language`) and the
ones prefixed with `Irisqo-Forward-`, without the prefix; the caller's own `Authorization`, `Cookie`
and other headers are not forwarded.
```
POST {{host}}/to/https://example.com/hook
Authorization: Bearer token-for-irisqo
Irisqo-Forward-Authorization: Bearer token-for-the-target
Content-Type: application/json
```

The forwarded headers are stored for the target and never given back: the API
(`GET /api/v1/jobs/{id}`) and the UI show their names, and the values of the open ones only
(`Content-Type`, `Content-Encoding`, `Content-Language`, `Accept`, `Accept-Encoding`,
`Accept-Language`, `User-Agent`, `Cache-Control`); any other value reads `••••`.

Every delivery also carries:
- `Irisqo-Job-Id` - the job, the same on every retry: delivery is at least once, use it to drop duplicates;
- `Irisqo-Retry` - the attempt, from 0;
- `Irisqo-Schedule-Id` - the schedule of a schedule run (every run is a new job);
- `traceparent`/`tracestate` - the trace context (see Telemetry).

### Retry
Automatically retry requests that fail by adding the `_retry` query parameter.
```
GET {{host}}/to/https://postman-echo.com/get?_retry=3
```

Retry Backoff: `N|fixed|S` waits `S` seconds before every retry (`N` alone retries at once),
`N|fibonacci|S` waits `S` × 1, 1, 2, 3, 5, … and `N|exponential|S` waits `S` × 1, 2, 4, 8, …;
an optional fourth part caps the backoff delay (`N|exponential|S|M`, seconds)
```
GET {{host}}/to/https://postman-echo.com/get?_retry=3|fixed|5

GET {{host}}/to/https://postman-echo.com/get?_retry=3|fibonacci|5

GET {{host}}/to/https://postman-echo.com/get?_retry=8|exponential|2|300
```

In JSON (`meta` of `POST /api/v1/jobs`, `enqueue_v1`): `retry` (`fixed`, `fibonacci`, `exponential`),
`retry_count`, `retry_delay`, `retry_max_delay`.

Retried: network errors, timeouts, `408`, `425`, `429` and `5xx`. Any other `4xx` fails the job at once.
Delays get ±20 % jitter, never come sooner than the response `Retry-After` (seconds or HTTP-date),
and are capped at 24 hours unless the policy sets them (`retry_delay` of `fixed`, `retry_max_delay`).

### Timeout
`_timeout` sets the request timeout in milliseconds (default `--timeout`, 3000), capped by `--max-timeout` (default 300000).

### Delay
Delay a request by adding the `_delay` or `_delay_until` [Unix Time] query parameter.
```
GET {{host}}/to/https://postman-echo.com/get?_delay=5
```

### Jobs and schedules
```
POST   {{host}}/api/v1/jobs                       a job as JSON: {meta, headers, body, key} (200, the job's id)
GET    {{host}}/api/v1/jobs/{id}                  the job
GET    {{host}}/api/v1/jobs/{id}/result           its result
GET    {{host}}/api/v1/jobs/{id}/history          its history
POST   {{host}}/api/v1/jobs/{id}/cancel           a pending job ends as cancelled (409 when running or done)
POST   {{host}}/api/v1/jobs/{id}/retry            a processed job runs once more (409 when pending or running)
DELETE {{host}}/api/v1/jobs/{id}
POST   {{host}}/api/v1/schedules/{id}/pause       no more runs, the pending one is cancelled
POST   {{host}}/api/v1/schedules/{id}/resume      the next run is scheduled
PUT    {{host}}/api/v1/schedules/{id}             declare a schedule (a new one, or replace its rule and job): {meta, headers, body, cron | interval, tz, until}
DELETE {{host}}/api/v1/schedules/{id}
```

### Errors
Errors are `application/problem+json` ([RFC 9457](https://www.rfc-editor.org/rfc/rfc9457)).
Invalid job parameters are rejected with `400` and every problem at once:
```json
{
  "type": "https://github.com/vobradovich/irisqo/blob/main/docs/problems.md#invalid-parameters",
  "title": "Invalid parameters",
  "status": 400,
  "detail": "2 parameters are invalid.",
  "errors": [
    { "parameter": "_delay", "detail": "must be a number of seconds" },
    { "parameter": "_cron", "detail": "needs 5 or 6 fields, got 4: e.g. */15 * * * *" }
  ],
  "trace_id": "4bf92f3577b34da6a3ce929d0e0e4736"
}
```
A missing job, result or schedule is a `404` with a `detail` (a `GET` of a job not processed yet has
no result either; a `DELETE` of a missing one stays `204`). Other errors carry only their status,
with `trace_id`: see [docs/problems.md](docs/problems.md).

### Authentication
With `--api-token` (better `IRISQO_API_TOKEN`: a flag shows in `ps`) the API (`/to/*`, `/api/v1/*`)
asks for `Authorization: Bearer <token>`; without it or with a wrong one the answer is a `401`
problem with `WWW-Authenticate: Bearer`. Several tokens, comma separated, let a new one roll out
before the old one goes; each needs at least 16 characters (`openssl rand -hex 32`).

```http
POST {{host}}/to/https://postman-echo.com/post
Authorization: Bearer {{token}}
```

The caller's `Authorization` stays here: only `Irisqo-Forward-*` headers reach the target.
`/live` and `/ready` need no token. Without a token the API and the UI are open, and an instance
outside development says so in its log at start.

The UI asks for the same token at `/ui/login` and keeps a pass in an `HttpOnly` cookie: the token's
HMAC of an expiry a week ahead, not the token itself, `SameSite=Lax`, and `Secure` behind an HTTPS
proxy (`X-Forwarded-Proto: https`). A pass lasts until it expires or its token is removed from
`--api-token`; the button in the header logs out.

### Targets
Outside development a job cannot call a non-public address: loopback, the private networks
(`10/8`, `172.16/12`, `192.168/16`, `fc00::/7`), link-local (`169.254/16` with the cloud metadata
service, `fe80::/10`), CGNAT, multicast and the other reserved ranges. An IP address or `localhost`
in the URL is refused when the job is created (`400`, `url`: `is a private address`). A host name is
checked when it is resolved, and the connection goes only to the addresses that pass: a job whose
name resolves only to private addresses fails without retries (`Blocked target`). Redirects are not
followed.

`--allow-targets 10.1.0.0/16,192.168.1.10` (`IRISQO_ALLOW_TARGETS`) allows these networks anyway;
`--env development` allows every address.

## UI
`/ui` serves a server-rendered UI (`/` sends a browser there): Askama templates, [htmx](https://htmx.org) for the partial updates and
[Pico CSS](https://picocss.com) for the base styles. Both are compiled into the binary (`assets/`), so
there is nothing to fetch from a CDN, and the pages carry a strict `Content-Security-Policy`
(no inline scripts or styles). The New job form builds the same `/to/` request as the API and goes
through the same validation; its preview shows that request.
The theme follows the system setting; the switch in the header picks light or dark instead, kept in
the browser (`assets/theme.js`, the only script besides htmx).
A request of the UI that changes something (a form, cancel, delete, pause) must come from the UI
itself: the browser's `Sec-Fetch-Site`, or else its `Origin` against the host (`X-Forwarded-Host`
behind a proxy). A page of another site cannot make a user's browser post to it (`403`).

[![Jobs: the queue stats and the jobs by state](docs/screenshots/jobs.png)](docs/screenshots/jobs.png)

<table>
  <tr>
    <td width="50%"><a href="docs/screenshots/job.png"><img src="docs/screenshots/job.png" alt="Job: the request, the result and the history"></a><br><sub>Job</sub></td>
    <td width="50%"><a href="docs/screenshots/new-job.png"><img src="docs/screenshots/new-job.png" alt="New job: the form and the same request with the API"></a><br><sub>New job</sub></td>
  </tr>
  <tr>
    <td width="50%"><a href="docs/screenshots/schedules.png"><img src="docs/screenshots/schedules.png" alt="Schedules: rule, next and last run"></a><br><sub>Schedules</sub></td>
    <td width="50%"><a href="docs/screenshots/instances.png"><img src="docs/screenshots/instances.png" alt="Instances: the processes on the queue"></a><br><sub>Instances</sub></td>
  </tr>
</table>

- **Jobs**: the queue stats and the jobs by state (scheduled, retrying, enqueued, processing,
  completed, failed, cancelled), searchable by id, key or URL, refreshed every 2 s; a row
  opens its history in place.
- **Job**: the request, the result, the history; retry, cancel, delete. With `--trace-url`
  (e.g. `http://localhost:5080/web/traces?trace_id={trace_id}`) the result links to its trace.
- **Schedules**: rule, next and last run; pause, resume, delete.
- **Instances**: the irisqo processes on this queue: state, last heartbeat, start, the jobs
  each holds and its results of the last 24 hours, refreshed every 5 s.
- **New job**: the form.

## Running

```bash
IRISQO_DB_URL=postgres://user:pass@localhost/irisqo cargo run --release -- --help
```

| Option | Default | |
|---|---|---|
| `--port` | 8102 | API port |
| `--workers` | 16 | worker tasks; each holds and runs up to `--prefetch` jobs at once |
| `--db-url` | — | Postgres URL, `postgres://user:pass@host/db`; better `IRISQO_DB_URL`: a flag shows in `ps`. Required |
| `--db-connections` | 32 | pool size per instance, keep the sum over instances below Postgres `max_connections` |
| `--db-schema` | irisqo | Postgres schema of the tables and functions, created when missing; `IRISQO_DB_SCHEMA` |
| `--interval` | 200 | ms before the first re-poll of an empty queue; the pause doubles with each empty poll and starts over when a claim finds jobs |
| `--max-interval` | 1000 | ms, longest pause between polls of an empty queue |
| `--prefetch` | 4 | jobs a worker holds and runs at once; in flight in all: `workers x prefetch` (64 by default), one fetch takes that many |
| `--timeout` / `--max-timeout` | 3000 / 300000 | default and max request timeout, ms |
| `--retention-hours` | 168 | processed jobs older than this are deleted, `0` keeps them |
| `--trace-url` | — | UI link to a trace, `{trace_id}` is replaced |
| `--shutdown-timeout` | 20 | s the running jobs get to finish on shutdown; keep it under the orchestrator grace period (30 s in Kubernetes) |
| `--api-token` | — | bearer tokens of the API, comma separated, see [Authentication](#authentication) |
| `--allow-targets` | — | private addresses and networks a job may call anyway, see [Targets](#targets) |
| `--env` | production | `development`, `staging` or `production`: development serves `/echo` and `/error`; also `deployment.environment.name` in the telemetry |

Each option can also be set by an `IRISQO_` variable, in the environment or a `.env` file:
`--max-timeout` is `IRISQO_MAX_TIMEOUT`, `--env` is `IRISQO_ENV`. The command line wins.

`--max-body-size` (`IRISQO_MAX_BODY_SIZE`) limits the full HTTP request body when submitting a job.
The default is 65536 bytes (64 KiB). For JSON jobs this includes the metadata, target headers and payload.
The worker reads up to 10 MiB of the target's response body.

The database URL is required: set `IRISQO_DB_URL` (better than `--db-url`: a flag shows the password in `ps`).

### Telemetry
Traces, metrics and logs go out over OTLP (HTTP/protobuf), configured by the standard env vars:

| Env var | |
|---|---|
| `OTEL_EXPORTER_OTLP_ENDPOINT` | enables the export of all three signals, e.g. `http://localhost:5080/api/default` |
| `OTEL_EXPORTER_OTLP_HEADERS` | e.g. `Authorization=Basic%20...` |
| `OTEL_{TRACES,METRICS,LOGS}_EXPORTER` | `otlp`, `console` or `none`, per signal |
| `OTEL_METRIC_EXPORT_INTERVAL` | ms, default 60000 |
| `OTEL_SERVICE_NAME` | default `irisqo` |

`docker-compose.dev.yml` runs Postgres and [OpenObserve](https://github.com/openobserve/openobserve)
(OSS, one container for traces, metrics and logs); the exports for it are in the file.
OTLP is plain HTTP: a local collector/agent or OpenObserve.

#### Traces
Jobs continue the trace of the request that created them (W3C `traceparent`/`tracestate`):

- every attempt is a `job.run` span, a child of that request (so retries are siblings),
  with `job.id`, `job.retry`, `job.schedule_id` and `job.outcome` (`completed`, `failed`, `retry`);
- the outgoing request is a client span, and the receiver gets its `traceparent`/`tracestate`;
- every run of a schedule starts its own trace, with a link to the request that created the
  schedule (one trace per schedule would grow forever);
- the result (`/api/v1/jobs/{id}/result`) has the `trace_id` and `span_id` of its attempt.

#### Metrics
| Metric | Type | Attributes |
|---|---|---|
| `http.server.request.duration` | histogram, s | `http.request.method`, `http.route`, `http.response.status_code` |
| `http.server.active_requests` | up-down counter | `http.request.method` |
| `http.client.request.duration` | histogram, s | `http.request.method`, `server.address`, `server.port`, `http.response.status_code`, `error.type` |
| `db.client.connection.count` | gauge | `db.client.connection.state` (`idle`, `used`) |
| `db.client.connection.max` | gauge | |
| `irisqo.jobs.created` | counter | `job.type` (`immediate`, `delayed`, `scheduled`) |
| `irisqo.job.attempt.duration` | histogram, s | `job.outcome` (`completed`, `failed`, `retry`) |
| `irisqo.jobs.in_flight` | up-down counter | attempts running on the instance |
| `irisqo.queue.depth` | gauge | `state` (`scheduled`, `enqueued`, `processing`), the whole queue, every 15 s |
| `irisqo.queue.lag` | gauge, s | how long the oldest due job has been waiting to start |
| `irisqo.locks.expired` | counter | jobs released by the lock reaper |
| `irisqo.instances.expired` | counter | instances declared dead |
| `irisqo.retention.deleted` | counter | `kind` (`jobs`, `schedules`, `instances`) |

`irisqo.queue.*` describe the whole queue: every instance reports the same values, aggregate them
with `max`, not `sum`.

#### Logs
Logs go to stdout and to OTel, both filtered by `RUST_LOG`; a log written inside a span carries
its `trace_id`/`span_id`. Exported spans do not depend on `RUST_LOG`.

### Delivery guarantees
At least once. Every instance sends a heartbeat; jobs of an instance silent for 30 s are released,
and so is any job locked for longer than `--max-timeout` + 60 s (a failed result write, a hung worker).
A late result of such an attempt is rejected: a job is completed only by its current attempt
(instance + retry number). On `SIGINT`/`SIGTERM` the instance stops taking jobs, finishes the ones in
flight (up to `--shutdown-timeout`), writes their results and releases the rest: an abandoned
attempt runs again elsewhere.

## Development

```bash
# unit tests
cargo test

# integration tests: every test gets its own fresh database on this server
DATABASE_URL=postgres://postgres@localhost/postgres cargo test --features integration-tests

# the same checks as CI
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
cargo deny check
cargo machete
```
