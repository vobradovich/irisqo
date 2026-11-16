# Callbacks

A job can name a **callback**: a request (a `POST`, or a `PUT`) that irisqo makes when the job
reaches its outcome. The callback either reports the outcome to you (a signed JSON webhook,
[Standard Webhooks](https://github.com/standard-webhooks/standard-webhooks/blob/main/spec/standard-webhooks.md))
or relays the job's response to the next service as it is.

A callback is a job itself: it has its own retries, timeout, history, trace and page in the UI,
and it is linked to the job that made it (`parent_id`).

Event types are `irisqo.job.completed`, `irisqo.job.failed` and `irisqo.job.cancelled`.

## Modes

`callback.mode` names the format of the delivery.

| `mode` | Fires on | Body sent | Headers |
|---|---|---|---|
| `webhook` (default) | `completed`, `failed`, `cancelled` | JSON, as Standard Webhooks: `type`, `timestamp`, `data` | `webhook-*` |
| `relay` | `completed` by default; any of the types with `types` | the response of the job, byte for byte, on `completed`; no body on the others | `Irisqo-*` |

A job has at most one callback, and an outcome makes at most one callback job.

- **Want to know how it ended?** `webhook`.
- **Want the result to go on to the next service?** `relay`. A job that did not complete relays
  no body, only headers, and only when `types` asks for it.

The mode defines the payload: `relay` sends the response of the job as it is, `webhook` builds
its own JSON. Which outcomes make a callback is not the mode's: `types` chooses.

## Request

The callback is an object, `callback`, of the job. In `/to/` its fields are query parameters with
`_callback_`, as the job's own are with `_`.

| JSON (`callback.`) | `/to/` | Meaning | When absent |
|---|---|---|---|
| `url` | `_callback_url` | `http` or `https` URL; sets the callback | no callback |
| `mode` | `_callback_mode` | `webhook` or `relay` | `webhook` |
| `method` | `_callback_method` | `POST` or `PUT` | `POST` |
| `types` | `_callback_types` (comma separated) | the types that make a callback (see Types) | `webhook`: `irisqo.job.completed`, `irisqo.job.failed`, `irisqo.job.cancelled`; `relay`: `irisqo.job.completed` |
| `headers` | `Irisqo-Callback-*` headers | headers for the callback | none |
| `retry` (`policy`, `count`, `delay`, `max_delay`, as the `retry` of the job) | `_callback_retry` (`N\|kind\|S\|M`, as `_retry`) | retry policy | the default of a callback (see Delivery), not the job's |
| `timeout` | `_callback_timeout` | milliseconds, at least 1 | the default of a callback (see Delivery), not the job's |
| `flow_key` | `_callback_flow_key` | flow of the callback, as `flow_key` | none (not inherited) |
| `priority` | `_callback_priority` | an integer; higher runs first, among the jobs without a flow or in its flow | 0 (not inherited) |

- The method is `POST` unless `method` says `PUT`; any other method is refused.
- `callback` needs its `url`; without it the callback is refused.
- `retry` is an object like the `retry` of a job: `policy` is `fixed`, `fibonacci` or
  `exponential`, with `count`, `delay` and `max_delay`; a policy without its `policy` is
  refused. **`{"policy": "fixed", "count": 0}` means no retries** (`_callback_retry=none` in
  `/to/`); it is not the same as leaving the policy out, which takes the default of a callback.
- `flow_key` lets a receiver be protected like any target: a concurrency or rate
  limit on `https://ship.example` is a flow. It is not inherited: the job's flow limits the
  job's target, the callback goes elsewhere.
- A callback runs as soon as it is made; there is no callback `delay` or `at`.
- `types` takes the full types, one spelling everywhere (`completed` alone is refused);
  an unknown type is refused, and so is an empty list (no callback is no `url`).
- `headers` are kept like the forwarded headers of a job: the API and the UI show
  their names, and the values of the open ones only (`Content-Type`, `Accept`...); any other
  value reads `••••`.

### Headers of a callback

`callback.headers` must be headers that can be sent:

- a name is a header name (letters, digits and ``!#$%&'*+-.^_`|~``) and a value has no control
  characters;
- not the names of the target request that the client sets itself (`Host`, `Content-Length`,
  `Transfer-Encoding`, `Connection`, `Traceparent`, `Tracestate`), and not the names that irisqo
  sets itself: `Webhook-*` and `Irisqo-*`;
- the one exception is `Irisqo-Forward-<name>` with a `<name>` that may be forwarded (not one of
  the above): it is for the next irisqo of a [chain](#a-chain-through-irisqo), which forwards
  it to its target as `<name>`; its value is shown as `••••`, as any secret;
- names are case-insensitive and cannot repeat.

The API (`POST /api/v1/jobs`, `/to/`) refuses a request that breaks this with a 400.
`enqueue_v1` does not check the names: a job that has such a header ends as it would, and
**no callback is made** for it (the log of the worker says which header).

### JSON (`POST /api/v1/jobs`)

```json
{
  "sink": "http", "method": "POST", "url": "https://pay.example/charge",
  "retry": { "policy": "exponential", "count": 5, "delay": 2 },
  "key": "order-42",
  "callback": {
    "url": "https://me.example/hooks/irisqo",
    "headers": { "Authorization": "Bearer cb-secret" },
    "retry": { "policy": "fixed", "count": 10, "delay": 30 }
  }
}
```

### `/to/`

```
POST {{host}}/to/https://pay.example/charge?_retry=5|exponential|2&_key=order-42&_callback_url=https://me.example/hooks/irisqo&_callback_retry=10|fixed|30
Irisqo-Callback-Authorization: Bearer cb-secret
```

`Irisqo-Callback-*` headers go to the callback without the prefix, as `Irisqo-Forward-*` go to
the target.

### `enqueue_v1`

Takes the same `callback` object as the JSON request, with the same checks, except the
names of `callback.headers` (see above).

## Success

A job is **`completed`** when its last attempt got an HTTP response with a 2xx status, or when
its sink is `noop`. Anything else is a failure: a 3xx, 4xx or 5xx status, a timeout, a
connection error, a response body over `--max-response-size`, a blocked target. One rule for
every job, callbacks included.

irisqo does not follow redirects, so a 3xx response is a failure, and it is not worth another
attempt: it ends the job at once, as a 4xx other than 408, 425 and 429 does.

## `webhook`: Standard Webhooks

### Types

| `type` | When |
|---|---|
| `irisqo.job.completed` | the job completed |
| `irisqo.job.failed` | an attempt failed and no other one follows (no retries left, or the error is not worth one) |
| `irisqo.job.cancelled` | a user cancelled the job while it waited: for its first attempt or for the next one |

A type is a dot-separated name that always has the same payload; a receiver can route on the
prefix (`irisqo.job.*`).

**`callback.types`** says which types make a callback. The filter works when the outcome is
recorded: a type that is not listed makes no callback job, and nothing is written for it.
`relay` takes any of the types too: on a type that is not `irisqo.job.completed` it sends no
body, only the headers (see `relay`).

### Body

The body says what happened (`type`, `timestamp`), which job it happened to (`data.job`), and
how its last attempt ended (`data.result`). `result` is the same JSON that
`GET /api/v1/jobs/{id}/result` gives.

```jsonc
// irisqo.job.completed
{
  "type": "irisqo.job.completed",
  "timestamp": "2026-11-13T10:00:03Z",    // ISO 8601: when the outcome was recorded
  "data": {
    "job": {
      "id": 42,                           // the job that made the callback
      "key": "order-42",                  // when the job has one
      "schedule_id": "01J…",              // when the job is a run of a schedule
      "method": "POST",
      "url": "https://pay.example/charge",
      "attempt": 2                        // the last attempt, from 0
    },
    "result": {
      "meta": {
        "result": "http", "status_code": 201, "version": "HTTP/1.1",
        "trace_id": "4bf92f3577b34da6a3ce929d0e0e4736", "span_id": "00f067aa0ba902b7"
      },
      "headers": { "content-type": "application/json" },
      "body": "eyJvcmRlcl9pZCI6N30="        // base64
    }
  }
}
```

```jsonc
// irisqo.job.failed: a 5xx after the last retry
{ "type": "irisqo.job.failed", "timestamp": "…",
  "data": { "job": { "id": 42, "attempt": 5, … },
            "result": { "meta": { "result": "http", "status_code": 503, … },
                        "headers": { "retry-after": "2" }, "body": "YnVzeQ==" } } }

// irisqo.job.failed: no response
{ "type": "irisqo.job.failed", "timestamp": "…",
  "data": { "job": { "id": 42, "attempt": 5, … },
            "result": { "meta": { "result": "timeout", … }, "headers": null, "body": "" } } }

// irisqo.job.failed: a 2xx response with a body over the limit
{ "type": "irisqo.job.failed", "timestamp": "…",
  "data": { "job": { "id": 42, "attempt": 0, … },
            "result": { "meta": { "result": "http", "status_code": 200,
                                  "error": "response too large: 300 bytes, the limit is 100", … },
                        "headers": { … }, "body": "" } } }

// irisqo.job.cancelled
{ "type": "irisqo.job.cancelled", "timestamp": "…",
  "data": { "job": { "id": 42, "attempt": 1, … },
            "result": { "meta": { "result": "cancelled" }, "headers": null, "body": "" } } }
```

| `type` | `data.result.meta.result` |
|---|---|
| `irisqo.job.completed` | `http` (2xx), `none` (a `noop` job) |
| `irisqo.job.failed` | `http` (not 2xx, or a body over the limit: `meta.error` says so), `timeout`, `error` (no response: connection, blocked target...; the text is in `meta.error`) |
| `irisqo.job.cancelled` | `cancelled` |

- `data.result.headers` holds every response header; `null` without a response.
- `data.result.body` is the response body in base64; `""` without one, and for a body over the
  limit (the status and the headers are kept).
- `data.result.meta.trace_id` / `span_id` are the trace of the last attempt; a cancelled job
  has none.
- Fields of `data.job` that do not apply are left out (`key`, `schedule_id`, and `method`,
  `url` for a `noop` job). Fields are only ever added: ignore the ones you do not know.

### Headers

The headers are the ones of the standard, nothing else of irisqo's:

```
POST https://me.example/hooks/irisqo
Content-Type: application/json
webhook-id: cb_01jc3k8x2m9q5r7t4v6w8y0z1a   ← the key of the callback job; the same on every retry
webhook-timestamp: 1763028009             ← unix seconds, of this delivery; new on every retry
webhook-signature: v1,g0hM9SsE+OTPJTGt/tmIKtSyZlE3uFJELVlNIOLJ1OE=     ← see Signature
traceparent: …
Authorization: Bearer cb-secret           ← from callback.headers
```

- `webhook-id` is the key of the callback job, `cb_` and a new lowercase ULID, made when the
  callback is made: use it to drop duplicates. It is the value of `Irisqo-Job-Key` in `relay`.
  It is a key and not the id of the job: the id is a number inside one database, the same in
  another deployment of irisqo, and a receiver cannot tell the two apart by it. The standard
  asks that the id has no `.`, which `cb_` and a ULID have not.
- What `relay` says in `Irisqo-Parent-*` is in the body here: `data.job.id`, `data.job.key`.
- `webhook-timestamp` is the time of the delivery; `timestamp` in the body is the time of the
  outcome. They differ after a retry.
- No `Irisqo-*` header is sent in this mode.

## `relay`

On `completed` (the default), the response of the job goes on to the callback URL:

```
POST https://ship.example/orders
Content-Type: application/json           ← from the response
Irisqo-Job-Id: 44                         ← the callback job, as on every delivery
Irisqo-Job-Key: cb_01jc3k8x2m9q5r7t4v6w8y0z1a   ← its key: the value of webhook-id in a webhook
Irisqo-Attempt: 0
Irisqo-Parent-Id: 42
Irisqo-Parent-Key: order-42               ← when the job has a key
Irisqo-Parent-Status: 201                 ← the HTTP status, when the job got a response
Irisqo-Event-Type: irisqo.job.completed   ← the type: route on it without the body
Irisqo-Timestamp: 1763028009              ← unix seconds, of this delivery; new on every retry
Irisqo-Signature: v1,K5oZfzN95Z9UVu1…     ← see Signature
traceparent: 00-4bf92f…-…-01              ← the trace of the job goes on
Authorization: Bearer for-ship            ← from callback.headers

{"order_id": 7, "amount": 990}            ← the body of the response, byte for byte
```

- **Headers taken from the response:** `Content-Type`, `Content-Encoding`, `Content-Language`
  only, the same body headers that `/to/` takes from a request. `Set-Cookie`, `Authorization`,
  `Location` and the rest stay with irisqo: they were meant for irisqo, not for the next
  service. To see all of them, use `webhook`.
- In `relay` `Irisqo-Job-Id`, `Irisqo-Job-Key` and `Irisqo-Attempt` are about the callback job
  itself (its own key, for dropping duplicates), and `Irisqo-Parent-*` about the job that made it, the
  `data.job` of a webhook. `Irisqo-Parent-Key` lets a receiver find its own record
  (`order-42`) without parsing the body.
- An empty response (204, `noop`) relays an empty body.
- A header of `callback.headers` wins over one taken from the response.
- The relayed request is the end: a callback has no callback of its own.

### `relay` of a job that did not complete

With `callback.types` that name `irisqo.job.failed` or `irisqo.job.cancelled`, `relay` sends
the headers and **no body**: there is no response to relay (a failed one is for `webhook`, which
gives it whole). Nothing is taken from the response either, so no `Content-Type`:

```
POST https://ship.example/orders/failed
Irisqo-Job-Id: 45
Irisqo-Job-Key: cb_01jc3k8x2m9r6s8u5w7x9z1b2c
Irisqo-Attempt: 0
Irisqo-Parent-Id: 42
Irisqo-Parent-Key: order-42
Irisqo-Parent-Status: 503                 ← only when the job got a response: not on timeout, error, cancelled
Irisqo-Event-Type: irisqo.job.failed
Irisqo-Timestamp: 1763028009
Irisqo-Signature: v1,…
```

`Irisqo-Event-Type` says which type it is, so a receiver routes on the header alone.

### A chain through irisqo

A callback has no callback, but a chain A → B → C needs none: relay the response of A to
irisqo itself, to `/to/` of the next step. The body of A becomes a new job to B, with its own
retries, timeout and callback:

```json
{
  "sink": "http", "method": "POST", "url": "https://pay.example/charge",
  "callback": {
    "mode": "relay",
    "url": "https://irisqo.example/to/https://ship.example/orders?_retry=5|exponential|2&_callback_url=https://me.example/hooks/irisqo",
    "headers": {
      "Authorization": "Bearer <irisqo api token>",
      "Irisqo-Forward-Authorization": "Bearer for-ship"
    }
  }
}
```

- `Authorization` is for irisqo, `Irisqo-Forward-*` for B, as in any `/to/` request. The
  `Content-Type` of A's response reaches B as well.
- The job to B is a new job, not a callback: its `parent_id` is empty, and the link to A is
  the trace (`traceparent` goes on) and `Irisqo-Parent-Id` in the request to `/to/`.
- irisqo calls itself as a target: when its address is private, allow it with
  `--allow-targets`, or use its public address.

## Signature

Both modes sign the delivery the same way: the values of the headers are the same, only the
names differ.

| | `webhook` | `relay` |
|---|---|---|
| key of the callback job | `webhook-id` | `Irisqo-Job-Key` |
| time | `webhook-timestamp` | `Irisqo-Timestamp` |
| signature | `webhook-signature` | `Irisqo-Signature` |

- **Key.** `--signing-key` (`IRISQO_SIGNING_KEY`), as the standard writes a symmetric key:
  `whsec_` and the base64 of 24 to 64 random bytes (`openssl rand -base64 32`). The key is the
  server's, one for every callback. The option takes several keys, comma separated, for rotation.
- **What is signed.** `{key}.{timestamp}.{body}`: the key of the callback job and the time as
  they are in the headers of this delivery, the body as the bytes sent (not parsed and written
  again). The key has no `.` in it (`cb_` and a ULID). Not to be mixed up with the signing key
  above: this one is the key of the job. The id of the job is not signed over and not used for
  a signature at all: it is an id inside one database, not unique.
- **How.** HMAC-SHA256 with the key (the bytes of the base64 part, without `whsec_`), the
  result in base64, written `v1,<base64>`.
- **Rotation.** With several keys the header holds one `v1,…` per key, separated by a space,
  the first key first. The receiver accepts the delivery when any of them matches.
- **Every attempt.** The time and the signature are made when the delivery is sent, not when
  the callback is made: a retry has a new time and a new signature. A receiver should refuse a
  time that is too far from its own (five minutes, say), and keep the keys it has seen for as
  long.
- **The signature is not required.** Without `--signing-key` the key and the time are sent, the
  signature header is not. In production a key is recommended.
- A `webhook` receiver checks it with any Standard Webhooks library. A `relay` receiver does
  the same by hand: the recipe above, with the `Irisqo-*` names.
- Only callbacks are signed. The deliveries of the other jobs are not.

## Delivery

- **Made with the outcome.** A callback job is written in the same transaction that records
  the outcome of the job (`processed`) or its cancellation: a callback is neither lost nor
  doubled when an instance stops or a write is repeated. An attempt lost with a stopped
  instance records no outcome (the job is released and runs again), so it makes no callback;
  the attempt that ends the job does.
- **At least once.** A callback can arrive twice. Use `webhook-id` (`Irisqo-Job-Key` in
  `relay`), or the job (`data.job.id`, `Irisqo-Parent-Id`) with the type and the attempt, to
  drop duplicates.
- **Its own retries.** A callback that fails is retried with its policy and ends `failed` like
  any job. It does not change the status of the job that made it.
- **Its own defaults.** A callback is retried hard and for long, as the standard asks, and not
  as the job is: without `callback.retry` and `callback.timeout` it takes the default of a
  callback, the same in every mode: `exponential`, 10 retries, delay 300 s, longest delay
  86400 s (about three days in all, with jitter), and a timeout of 30 s (capped by
  `--max-timeout`). A job without retries does not lose its callback to one failure of the
  receiver.
- **Targets.** Callback URLs pass the same target checks as jobs (`--allow-targets`).
- **Taken at once.** The body of a callback is built when the outcome is recorded and stored
  with the callback job: deleting the job that made it (retention) does not change it. The
  time and the signature are the exception: they are made when the delivery is sent.

Other cases:

- A repeated `key` returns the existing job and makes no callback.
- Every run of a schedule carries the callback of the schedule's job.
- A run that its schedule takes back (the schedule is declared again, or paused) makes no
  callback; the history of the run says why. Only a user's cancel makes a `cancelled` callback.
- A manual retry of a finished job (cancelled ones included) runs a new attempt, and its
  outcome makes a new callback; `data.job.attempt` tells them apart.
- A callback job can be cancelled or retried by hand like any job.

## History

- **The job** gets no row for its callback: the row of the outcome (`completed`, `failed`,
  `cancelled`) is there already, and a retry has its own `retried` row.
- **The callback job** starts with `enqueued`, with the message `callback of #42`, and goes on
  like any job.

## API and UI

### API

- A job shows `parent_id` in `GET /api/v1/jobs/{id}`. A job with a `parent_id` is a callback.
- `GET /api/v1/jobs?parent_id=42` lists the callbacks of a job, oldest first, as
  `GET /api/v1/jobs/{id}` gives them. It comes in pages: `limit` (100 by default, 1000 at
  most) and `after`, the id of the last job of the previous page. A page with fewer jobs than
  `limit` is the last one.
- OpenAPI: the `callback_*` fields of the job request; the body of a `webhook` is the
  `CallbackWebhook` schema, for receivers in the SDKs.

### Job list

The **Source** column and filter show where a job comes from. A run of a schedule and a callback
are the same kind of link, and a job is never both (a callback has no `schedule_id`), so one
column holds either:

| The job is | Source shows |
|---|---|
| a run of a schedule | the schedule id, short (`01J…`), the whole id in its title |
| a callback | `↳ #42`, a link to the job that made it; title `callback of #42` |
| neither | nothing |

The **Source** filter: `Any`, `Schedule runs`, `Callbacks`, `Direct` (neither). `Direct` hides
the callbacks.

### Job page

- A callback: `Callback of #42`, a link, and its mode (`a deleted job` when the job that made it
  is gone).
- A job with a callback: the callback's URL, mode, retry and timeout (`callback.headers` as
  the job's own headers: names, and the values of the open ones), and its callback jobs with
  their status.

## Security

- **Signed deliveries.** The receiver checks the signature (see Signature) and needs no secret
  in the job. Without a signing key, a receiver can check a secret header that the job brings in
  `callback.headers` / `Irisqo-Callback-*`.
- `relay` sends the response body and its three body headers, plus `callback.headers` and the
  `Irisqo-*` delivery headers; no other header of the response leaves irisqo.
- `webhook` sends the whole result in the body, response headers included (`Set-Cookie` too):
  the receiver of a callback sees what irisqo saw. Point it at a service you trust, and sign.
- The values of `callback.headers` are never given back by the API or the UI, except the open
  ones; they are not written to the log.

## Limits

- A response body over `--max-response-size` (`IRISQO_MAX_RESPONSE_SIZE`, 64 KiB by default)
  fails the job and no body is kept (see Success), so a body that reaches a callback always
  fits: up to that size in `relay`, about a third more in a `webhook` (the same body in base64).
  The standard advises small payloads, about 20 KB: keep the limit low when the receiver of a
  `webhook` is strict.
