# irisqo

'irisqo' is a proof-of-concept service for making asynchronous HTTP requests with delays and retries, similar to 'zeplo.io' or 'qstash'.

### Design
- KISS (Keep It Simple)

### Features
- [x] Persistent queue backed by 'Postgres' with lock-free queries.
- [x] Scalability
- [x] Retries and Delays
- [ ] OpenTelemetry Tracing
- [ ] Schedules, Intervals, and Cron
- [ ] UI