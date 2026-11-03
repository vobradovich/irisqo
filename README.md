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
To queue a request, simply prefix a request with {{host}}/to/ and we’ll queue and forward the request with the exact same method/body/headers/query.
```
@host=http://localhost:8102

###
GET {{host}}/to/https://postman-echo.com/get
```

Response Headers
- `job-id` - this is the ID of the request job
- `schedule-id` - ID of the new schedule
- `external-id` - optional external ID of the request from `_id` query parameter.

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

### Tracing
Jobs continue the trace of the request that created them (W3C `traceparent`/`tracestate`):

- every attempt is a `job.run` span, a child of that request (so retries are siblings),
  with `job.id`, `job.retry`, `job.schedule_id` and `job.outcome` (`completed`, `failed`, `retry`);
- the outgoing request is a client span, and the receiver gets its `traceparent`/`tracestate`;
- every run of a schedule starts its own trace, with a link to the request that created the
  schedule (one trace per schedule would grow forever);
- the result (`/api/v1/jobs/{id}/result`) has the `trace_id` and `span_id` of its attempt.

Export is configured by the standard env vars: `OTEL_EXPORTER_OTLP_ENDPOINT` enables OTLP over
HTTP/protobuf (plain HTTP, e.g. a local collector or Jaeger, see `docker-compose.dev.yml`),
`OTEL_TRACES_EXPORTER=console` prints the spans, `OTEL_SERVICE_NAME` overrides `irisqo`.
`RUST_LOG` filters the logs only.

### Delivery guarantees
At least once. Every instance sends a heartbeat; jobs of an instance silent for 30 s are released,
and so is any job locked for longer than `--max-timeout` + 60 s (a failed result write, a hung worker).
A late result of such an attempt is rejected: a job is completed only by its current attempt
(instance + retry number). On `SIGINT`/`SIGTERM` the instance stops taking jobs, finishes the ones in
flight, writes their results and releases the rest.

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
