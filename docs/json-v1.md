# Job JSON v1

The one description of a job: the body of `POST /api/v1/jobs` and `PUT /api/v1/schedules/{id}`,
the argument of `enqueue_v1(job jsonb)`, and the `meta` of a job that `GET /api/v1/jobs/{id}`
returns. `/to/` and the "New job" form take the same fields as flat parameters.

## Request

```jsonc
{
  "url": "https://pay.example/charge",   // required
  "method": "POST",
  "headers": { "content-type": "application/json" },
  "body": { "order_id": 7 },
  "key": "order-42",
  "at": "2030-01-01T00:00:00Z",

  "retry": { "policy": "exponential", "count": 5, "delay": 2, "max_delay": 300 },
  "timeout": 10000,
  "flow_key": "payments",
  "priority": 10,

  "callback": {
    "url": "https://me.example/hooks/irisqo",
    "mode": "webhook",
    "headers": { "authorization": "Bearer cb-secret" },
    "retry": { "policy": "fixed", "count": 10, "delay": 30 }
  }
}
```

### Fields

| Field | Type | Default | Rules |
|-------|------|---------|-------|
| `url` | string | — | Required. `http://` or `https://` is the `http` sink; `noop://…` is the `noop` sink (completes at once, does nothing). |
| `method` | string | `POST` | HTTP method, any case (`post` is `POST`). `http` sink only. |
| `headers` | object of strings | — | Headers for the target. Names that differ only by case are refused. |
| `body` | any JSON | — | A string is sent as it is, any other value as JSON text (`content-type` defaults to JSON). |
| `body_base64` | string | — | Base64 of a binary body. Not together with `body`. |
| `key` | string | — | A repeated key returns the existing job. With `cron` or `interval`, the schedule id. |
| `at` | string or number | now | RFC 3339 time or unix seconds. Not together with `delay`. A time in the past runs now. |
| `delay` | number | — | Seconds from now. Not together with `at`. |
| `retry` | object or string | no retries | See [Retry](#retry). |
| `timeout` | number | `--timeout` (3000) | Milliseconds, at least 1. |
| `flow_key` | string | — | 1 to 128 characters of `a-z 0-9 . _ : -`, starting with a letter or digit. Empty is none. |
| `priority` | number | 0 | 32-bit integer. Higher runs first, among the jobs of its flow if it has one. |
| `callback` | object | no callback | See [Callback](#callback). |
| `cron` | string | — | See [Schedules](#schedules). |
| `interval` | number | — | See [Schedules](#schedules). |
| `tz` | string | `UTC` | See [Schedules](#schedules). |
| `until` | string or number | — | See [Schedules](#schedules). |

Unknown fields: the API ignores them, `enqueue_v1` refuses them (in the job, the callback and a
retry policy). `traceparent` and `tracestate` are taken from the request headers by the API;
`enqueue_v1` accepts them as fields.

### Retry

```jsonc
{ "policy": "exponential", "count": 5, "delay": 2, "max_delay": 300 }
```

| Field | Type | Rules |
|-------|------|-------|
| `policy` | string | `fixed`, `fibonacci` or `exponential`. |
| `count` | number | Retries before the job fails, 0 to 9999. |
| `delay` | number | Seconds, 0 to 604800. Required for `fibonacci` and `exponential`; `fixed` defaults to 0 (retry at once). |
| `max_delay` | number | Seconds, 0 to 604800. `fibonacci` and `exponential` only; 86400 when absent. |

The delay before retry *n* (0-based):

- `fixed`: `delay`;
- `fibonacci`: `delay` × 1, 1, 2, 3, 5, 8, …;
- `exponential`: `delay` × 1, 2, 4, 8, 16, …;

capped by `max_delay` and spread by ±20 % so that jobs failed together do not retry together.

The API and `/to/` also take the policy as text:

| Text | Object |
|------|--------|
| `none` | `{ "policy": "fixed", "count": 0 }` |
| `N` | `{ "policy": "fixed", "count": N }` |
| `N\|fixed\|S` | `{ "policy": "fixed", "count": N, "delay": S }` |
| `N\|fibonacci\|S` | `{ "policy": "fibonacci", "count": N, "delay": S }` |
| `N\|exponential\|S\|M` | `{ "policy": "exponential", "count": N, "delay": S, "max_delay": M }` |

`,` works as a separator too. `enqueue_v1` takes the object only.

### Callback

A request that irisqo makes when the job reaches its outcome. See [callbacks.md](callbacks.md).

| Field | Type | Default | Rules |
|-------|------|---------|-------|
| `url` | string | — | Required. `http://` or `https://`. |
| `method` | string | `POST` | `POST` or `PUT`, any case. |
| `mode` | string | `webhook` | `webhook` or `relay`. |
| `types` | array of strings | webhook: all three; relay: `irisqo.job.completed` | `irisqo.job.completed`, `irisqo.job.failed`, `irisqo.job.cancelled`. Not empty. |
| `headers` | object of strings | — | Headers of the callback request. |
| `retry` | object or string | exponential, 10 × 300 s, up to 86400 s | As [Retry](#retry); `none` is no retries. |
| `timeout` | number | 30000 | Milliseconds, at least 1. |
| `flow_key` | string | — | As the job's; not inherited from the job. |
| `priority` | number | 0 | As the job's. |

### Schedules

`cron` or `interval` (not both) repeats the job:

| Field | Rules |
|-------|-------|
| `cron` | A cron expression of 5 fields, or 6 with seconds first. |
| `interval` | Seconds between runs, at least 1. |
| `tz` | IANA zone of `cron` (`Europe/Berlin`), default UTC. Only with `cron` or `interval`. |
| `until` | As `at`: no run after it. Only with `cron` or `interval`. |
| `key` | The schedule id; a new ULID when absent. A repeated key changes the schedule. |

A schedule that would never run is refused.

`PUT /api/v1/schedules/{id}` takes the same body: the path `id` is the `key`, and `cron` or
`interval` is required. An invalid id is the error of `id`.

Declaring an existing schedule replaces its rule and job: the waiting run is cancelled and
replaced by a new job with a new id, even when the body is the same. A paused schedule is
resumed.

## Response

| Status | When |
|--------|------|
| `201 Created` | A new job, or the first run of a new schedule. |
| `200 OK` | The existing job of a repeated `key`, or the next run of an updated schedule. |
| `400 Bad Request` | Invalid fields: an RFC 9457 problem with `errors`, see [problems.md](problems.md). |

The body is the job (`id`, `key`, `schedule_id`); the same is in the `Location` and `Irisqo-*`
headers. `/to/` answers with the same statuses and headers, without a body.

Errors are named by field, for `/to/` too: `delay`, `retry`, `callback.url`, `callback.retry`.

## Stored meta

`meta` of a job as `GET /api/v1/jobs/{id}` returns it and `jobs.meta` stores it:

```jsonc
{
  "sink": "http",                 // "http" or "noop"
  "method": "POST",               // http only, upper case
  "url": "https://pay.example/charge",   // http only
  "retry": { "policy": "exponential", "count": 5, "delay": 2, "max_delay": 300 },
  "timeout": 10000,               // the request's, or --timeout
  "flow_key": "payments",
  "priority": 10,
  "callback": {
    "url": "https://me.example/hooks/irisqo",
    "mode": "webhook",
    "headers": { "authorization": "…" },   // the API shows the values of open headers only
    "retry": { "policy": "fixed", "count": 10, "delay": 30 }
  },
  "traceparent": "00-4bf92f35…-01",
  "tracestate": "…",
  "callback_as": "webhook"        // set on a callback job only, by the callback that made it
}
```

- `retry` is always stored as the object; absent means no retries (for a callback: its default).
- Absent optional fields are left out.
- `body`, `headers`, `key`, `schedule_id` and `parent_id` of a job are not in `meta`.

## `/to/`

`/to/{url}` takes the target as the path and the fields as `_`-prefixed query parameters:

| Parameter | Field |
|-----------|-------|
| `_delay`, `_at`, `_timeout`, `_retry`, `_flow_key`, `_priority`, `_key` | as in JSON |
| `_cron`, `_interval`, `_tz`, `_until` | as in JSON |
| `_callback_url`, `_callback_method`, `_callback_mode`, `_callback_types`, `_callback_retry`, `_callback_timeout`, `_callback_flow_key`, `_callback_priority` | `callback.*` |
| `Irisqo-Callback-*` headers | `callback.headers` |

The request's method, other query parameters, headers and body go to the target. `_retry`
and `_callback_retry` are the text form of a policy; `_callback_types` is a comma-separated
list. Errors are named as the JSON fields (`delay`, `callback.url`).
