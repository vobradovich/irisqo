# irisqo

`irisqo` is a proof-of-concept service for making asynchronous HTTP requests with delays and retries, similar to `zeplo.io` or `qstash`.

### Design
- Postgres Backend, Lock-Free Queries
- KISS (Keep It Simple, Stupid)

### Features
- [x] Persistent queue backed by `Postgres` with lock-free queries.
- [x] Scalability
- [x] Retries and Delays
- [ ] OpenTelemetry Tracing
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

### Delay
Delay a request by adding the `_delay` or `_delay_until` [Unix Time] query parameter.
```
GET {{host}}/to/https://postman-echo.com/get?_delay=5
```