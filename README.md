# irisqo

`irisqo` is a proof-of-concept service for making asynchronous HTTP requests with delays and retries, similar to `zeplo.io` or `qstash`.

### Design
- Postgres Backend, Lock-Free Queries
- KISS (Keep It Simple, Stupid)

### Features
- [x] Persistent queue backed by `Postgres` with lock-free queries.
- [x] Scalability
- [x] Retries and Delays
- [x] OpenTelemetry Tracing
- [x] Schedules, Intervals, and Cron
- [ ] UI

## API

### Queue
To queue a request, simply prefix a request with {{host}}/to/ and we’ll queue and forward the request with the exact same method/body/query.
```
@host=http://localhost:8102

###
GET {{host}}/to/https://postman-echo.com/get
```

Response Headers
- `job-id` - this is the ID of the request job
- `schedule-id` - ID of the new schedule
- `external-id` - optional external ID of the request from `_id` query parameter.

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

Retry Backoff
```
GET {{host}}/to/https://postman-echo.com/get?_retry=3|fixed|5

GET {{host}}/to/https://postman-echo.com/get?_retry=3|fibonacci|5
```

Retried: network errors, timeouts, `408`, `425`, `429` and `5xx`. Any other `4xx` fails the job at once.
Delays get ±20 % jitter, never come sooner than the response `Retry-After` (seconds or HTTP-date),
and are capped at 24 hours.

### Timeout
`_timeout` sets the request timeout in milliseconds (default `--timeout`, 3000), capped by `--max-timeout` (default 300000).

### Delay
Delay a request by adding the `_delay` or `_delay_until` [Unix Time] query parameter.
```
GET {{host}}/to/https://postman-echo.com/get?_delay=5
```

## Running

```bash
DATABASE_URL=postgres://user:pass@localhost/irisqo cargo run --release -- --help
```

| Option | Default | |
|---|---|---|
| `--port` | 8102 | API port |
| `--concurrency` | 64 | jobs in flight across all cores |
| `--db-connections` | 32 | pool size per instance, keep the sum over instances below Postgres `max_connections` |
| `--interval` | 1000 | ms between queue polls when the queue is empty |
| `--prefetch` | 8 | jobs assigned per fetch: `max(prefetch, concurrency / 4)` |
| `--timeout` / `--max-timeout` | 3000 / 300000 | default and max request timeout, ms |
| `--retention-hours` | 168 | processed jobs older than this are deleted, `0` keeps them |
| `--shutdown-timeout` | 20 | s the running jobs get to finish on shutdown; keep it under the orchestrator grace period (30 s in Kubernetes) |

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
