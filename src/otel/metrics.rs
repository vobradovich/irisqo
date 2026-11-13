//! Metrics: the HTTP ones follow the OpenTelemetry semantic conventions,
//! the queue ones are `irisqo.*`.

use std::{sync::OnceLock, time::Instant};

use axum::{extract::MatchedPath, extract::Request, middleware::Next, response::Response};
use opentelemetry::{
    KeyValue, global,
    metrics::{Counter, Gauge, Histogram, UpDownCounter},
};
use sqlx::{Pool, Postgres};

/// Seconds; the semantic conventions buckets for HTTP durations.
const HTTP_BUCKETS: [f64; 14] = [
    0.005, 0.01, 0.025, 0.05, 0.075, 0.1, 0.25, 0.5, 0.75, 1.0, 2.5, 5.0, 7.5, 10.0,
];
/// Seconds; a job attempt lasts up to `--max-timeout` (5 min by default).
const JOB_BUCKETS: [f64; 14] = [
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 300.0,
];

#[derive(Debug)]
pub struct Metrics {
    /// `http.server.request.duration` {http.request.method, http.route, http.response.status_code}
    pub http_server_duration: Histogram<f64>,
    /// `http.server.active_requests` {http.request.method}
    pub http_server_active: UpDownCounter<i64>,
    /// `http.client.request.duration` {http.request.method, server.address, server.port,
    /// http.response.status_code | error.type}
    pub http_client_duration: Histogram<f64>,
    /// `irisqo.jobs.created` {job.type: immediate | delayed | scheduled}
    pub jobs_created: Counter<u64>,
    /// `irisqo.job.attempt.duration` {job.outcome: completed | failed | retry}
    pub job_attempt_duration: Histogram<f64>,
    /// `irisqo.jobs.in_flight`: attempts running on this instance
    pub jobs_in_flight: UpDownCounter<i64>,
    /// `irisqo.locks.expired`: jobs released by the lock reaper
    pub locks_expired: Counter<u64>,
    /// `irisqo.instances.expired`: instances declared dead (their jobs released)
    pub instances_expired: Counter<u64>,
    /// `irisqo.retention.deleted` {kind: jobs | schedules | instances}
    pub retention_deleted: Counter<u64>,
    /// `irisqo.queue.depth` {state: scheduled | enqueued | processing}
    pub queue_depth: Gauge<u64>,
    /// `irisqo.queue.held`: the enqueued jobs that belong to a flow (a part of `enqueued`)
    pub queue_held: Gauge<u64>,
    /// `irisqo.queue.lag`: seconds the oldest due job without a flow has been waiting to start
    pub queue_lag: Gauge<f64>,
}

/// The instruments, created on first use from the global meter provider
/// (so it must be set before: `Telemetry::init`).
pub fn metrics() -> &'static Metrics {
    static METRICS: OnceLock<Metrics> = OnceLock::new();
    METRICS.get_or_init(|| {
        let meter = global::meter("irisqo");
        Metrics {
            http_server_duration: meter
                .f64_histogram("http.server.request.duration")
                .with_unit("s")
                .with_description("Duration of HTTP server requests")
                .with_boundaries(HTTP_BUCKETS.to_vec())
                .build(),
            http_server_active: meter
                .i64_up_down_counter("http.server.active_requests")
                .with_unit("{request}")
                .with_description("Number of active HTTP server requests")
                .build(),
            http_client_duration: meter
                .f64_histogram("http.client.request.duration")
                .with_unit("s")
                .with_description("Duration of HTTP client requests (job deliveries)")
                .with_boundaries(HTTP_BUCKETS.to_vec())
                .build(),
            jobs_created: meter
                .u64_counter("irisqo.jobs.created")
                .with_unit("{job}")
                .with_description("Jobs created through the API")
                .build(),
            job_attempt_duration: meter
                .f64_histogram("irisqo.job.attempt.duration")
                .with_unit("s")
                .with_description("Duration of job attempts, by outcome")
                .with_boundaries(JOB_BUCKETS.to_vec())
                .build(),
            jobs_in_flight: meter
                .i64_up_down_counter("irisqo.jobs.in_flight")
                .with_unit("{job}")
                .with_description("Job attempts running on this instance")
                .build(),
            locks_expired: meter
                .u64_counter("irisqo.locks.expired")
                .with_unit("{job}")
                .with_description("Jobs released by the lock reaper")
                .build(),
            instances_expired: meter
                .u64_counter("irisqo.instances.expired")
                .with_unit("{instance}")
                .with_description("Instances declared dead, their jobs released")
                .build(),
            retention_deleted: meter
                .u64_counter("irisqo.retention.deleted")
                .with_unit("{row}")
                .with_description("Rows deleted by the retention")
                .build(),
            queue_depth: meter
                .u64_gauge("irisqo.queue.depth")
                .with_unit("{job}")
                .with_description(
                    "Jobs in the queue, by state (the whole queue, not this instance)",
                )
                .build(),
            queue_held: meter
                .u64_gauge("irisqo.queue.held")
                .with_unit("{job}")
                .with_description(
                    "Enqueued jobs of a flow: they wait for the limits of their flow (a part of \
                     the enqueued jobs in irisqo.queue.depth)",
                )
                .build(),
            queue_lag: meter
                .f64_gauge("irisqo.queue.lag")
                .with_unit("s")
                .with_description(
                    "How long the oldest due job without a flow has been waiting to start \
                     (a flow holds its jobs by its limits, that is no lag)",
                )
                .build(),
        }
    })
}

/// `db.client.connection.count` {db.client.connection.state: idle | used} and
/// `db.client.connection.max` of the pool, observed at every export.
pub fn register_pool_metrics(pool: &Pool<Postgres>) {
    let meter = global::meter("irisqo");
    let observed = pool.clone();
    meter
        .u64_observable_gauge("db.client.connection.count")
        .with_unit("{connection}")
        .with_description("Connections of the pool, by state")
        .with_callback(move |observer| {
            let size = u64::from(observed.size());
            let idle = u64::try_from(observed.num_idle()).unwrap_or(u64::MAX);
            observer.observe(idle, &[pool_attr(), state_attr("idle")]);
            observer.observe(
                size.saturating_sub(idle),
                &[pool_attr(), state_attr("used")],
            );
        })
        .build();
    let max = u64::from(pool.options().get_max_connections());
    meter
        .u64_observable_gauge("db.client.connection.max")
        .with_unit("{connection}")
        .with_description("Max connections of the pool")
        .with_callback(move |observer| observer.observe(max, &[pool_attr()]))
        .build();
}

fn pool_attr() -> KeyValue {
    KeyValue::new("db.client.connection.pool.name", "irisqo")
}

fn state_attr(state: &'static str) -> KeyValue {
    KeyValue::new("db.client.connection.state", state)
}

/// Axum middleware for the HTTP server metrics.
pub async fn track_http_server(req: Request, next: Next) -> Response {
    let metrics = metrics();
    let method = KeyValue::new("http.request.method", req.method().as_str().to_owned());
    let route = req
        .extensions()
        .get::<MatchedPath>()
        .map(|path| KeyValue::new("http.route", path.as_str().to_owned()));
    let active = [method.clone()];
    metrics.http_server_active.add(1, &active);
    let start = Instant::now();
    let response = next.run(req).await;
    metrics.http_server_active.add(-1, &active);
    let mut attrs = vec![
        method,
        KeyValue::new(
            "http.response.status_code",
            i64::from(response.status().as_u16()),
        ),
    ];
    attrs.extend(route);
    metrics
        .http_server_duration
        .record(start.elapsed().as_secs_f64(), &attrs);
    response
}
