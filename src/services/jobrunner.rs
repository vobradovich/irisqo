use crate::{
    db,
    features::{
        self,
        results::{self, JobResult, is_retryable_status},
        schedules::JobSchedule,
    },
    models::{
        AppState, Error, JobEntry, JobRetry, JobRow, JobSink, JobWithRetry, MAX_RETRY_DELAY,
        targets, with_jitter,
    },
    otel::{self, HeaderInjector, metrics::metrics},
};
use bytes::Bytes;
use chrono::{DateTime, Utc};
use http_body_util::{BodyExt, Full, Limited};
use hyper::header::HeaderValue;
use opentelemetry::{
    Context, KeyValue, global,
    trace::{SpanKind, TraceContextExt},
};
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};
use tokio::time;
use tracing::{Instrument, field::Empty};
#[allow(unused_imports)]
use tracing::{debug, error, info, warn};
use tracing_opentelemetry::OpenTelemetrySpanExt;

const MAX_RESPONSE_BODY: usize = 10 * 1024 * 1024;

/// Final result of a job attempt, to be written to `processed`.
#[derive(Debug)]
pub struct Completion {
    pub entry: JobEntry,
    pub schedule_id: Option<String>,
    pub result: JobResult,
}

/// Runs a job and writes its result right away.
pub async fn job_run(app_state: &AppState, job_with_retry: JobWithRetry) {
    if let Some(completion) = job_execute(app_state, job_with_retry).await {
        complete(app_state, vec![completion]).await;
    }
}

/// Runs a job attempt. A retry is scheduled right here; a final result is returned
/// as a [`Completion`] for [`complete`] (so that the results can be written in batches).
///
/// The attempt is a `job.run` span, a child of the request that created the job
/// (`meta.traceparent`), so retries are siblings. A schedule run instead starts its
/// own trace with a link to that request: a schedule would grow one trace forever.
pub async fn job_execute(app_state: &AppState, job_with_retry: JobWithRetry) -> Option<Completion> {
    let meta = &job_with_retry.job.meta;
    let span = tracing::info_span!(
        target: otel::TRACING_TARGET,
        "job.run",
        otel.name = "job.run",
        otel.kind = ?SpanKind::Consumer,
        otel.status_code = Empty,
        job.id = job_with_retry.job.id,
        job.retry = job_with_retry.retry,
        job.schedule_id = job_with_retry.job.schedule_id.as_deref(),
        job.outcome = Empty,
    );
    let request =
        otel::context_from_trace_headers(meta.traceparent.as_deref(), meta.tracestate.as_deref());
    let parent = if job_with_retry.job.schedule_id.is_some() {
        let link = request.span().span_context().clone();
        if link.is_valid() {
            span.add_link(link);
        }
        Context::new()
    } else {
        request
    };
    _ = span
        .set_parent(parent)
        .inspect_err(|err| debug!("job.run set_parent error {:?}", err));
    let ids = otel::span_ids(&span);
    let metrics = metrics();
    metrics.jobs_in_flight.add(1, &[]);
    let start = Instant::now();
    let mut completion = job_attempt(app_state, job_with_retry)
        .instrument(span.clone())
        .await;
    metrics.jobs_in_flight.add(-1, &[]);
    let outcome = completion
        .as_ref()
        .map_or("retry", |c| c.result.processed_status());
    metrics.job_attempt_duration.record(
        start.elapsed().as_secs_f64(),
        &[KeyValue::new("job.outcome", outcome)],
    );
    span.record("job.outcome", outcome);
    span.record(
        "otel.status_code",
        if outcome == "completed" {
            "OK"
        } else {
            "ERROR"
        },
    );
    if let (Some(completion), Some((trace_id, span_id))) = (completion.as_mut(), ids) {
        completion.result.meta.trace_id = Some(trace_id);
        completion.result.meta.span_id = Some(span_id);
    }
    completion
}

async fn job_attempt(app_state: &AppState, job_with_retry: JobWithRetry) -> Option<Completion> {
    let instance_id = &app_state.instance_id;
    let JobWithRetry { job, retry } = job_with_retry;
    let entry = JobEntry { id: job.id, retry };
    let job_id = job.id;
    debug!({ instance_id, job_id, retry }, "==> run");
    let job_retry = job.meta.retry;
    let schedule_id = job.schedule_id.clone();
    let job_result = match job.meta.sink {
        JobSink::None => Ok(JobResult::default()),
        JobSink::Http(_) => job_run_http(app_state, job, retry).await,
    };
    let result = match job_result {
        Ok(result) => result,
        Err(err) => {
            let result = on_error(app_state, entry, job_retry, err).await?;
            warn!({ instance_id, job_id }, "====> job failed={:?}", &result.meta);
            result
        }
    };
    Some(Completion {
        entry,
        schedule_id,
        result,
    })
}

/// Pauses between repeats of a failed write: 250 ms doubling, 30 s in all (under the job lease).
fn write_pauses() -> impl Iterator<Item = Duration> {
    (0..7).map(|i| Duration::from_millis(250 << i))
}

/// Repeats a write that failed with a transient database error, after each of the `pauses`.
async fn write_with_retry<T, F, Fut>(
    pauses: impl IntoIterator<Item = Duration>,
    mut write: F,
) -> Result<T, Error>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, Error>>,
{
    for pause in pauses {
        match write().await {
            Err(err) if err.is_transient() => {
                warn!("write failed, again in {pause:?}: {err:?}");
                time::sleep(pause).await;
            }
            result => return result,
        }
    }
    write().await
}

/// Writes final results: one statement for all plain jobs, a transaction per scheduled
/// job (its result and the next run are written atomically).
pub async fn complete(app_state: &AppState, completions: Vec<Completion>) {
    let instance_id = &app_state.instance_id;
    let (scheduled, plain): (Vec<_>, Vec<_>) = completions
        .into_iter()
        .partition(|c| c.schedule_id.is_some());
    if !plain.is_empty() {
        let items: Vec<_> = plain.iter().map(|c| (c.entry, &c.result)).collect();
        let total = items.len() as u64;
        _ = write_with_retry(write_pauses(), || {
            results::processed_many(&app_state.pool, &items, instance_id)
        })
        .await
            .inspect(|&affected| {
                if affected < total {
                    // The lock was lost (lock expired, instance expired, job deleted)
                    warn!({ instance_id, total, affected }, "some jobs are not locked by this attempt, skip");
                }
            })
            .inspect_err(|err| {
                error!({ instance_id, total }, "results::processed_many error {:?}", err);
            });
    }
    for completion in scheduled {
        let job_id = completion.entry.id;
        _ = write_with_retry(write_pauses(), || {
            processed_scheduled(app_state, &completion)
        })
        .await
        .inspect_err(|err| error!({ instance_id, job_id }, "processed error {:?}", err));
    }
}

async fn job_run_http(app_state: &AppState, job: JobRow, retry: i32) -> Result<JobResult, Error> {
    let job_id = job.id;
    // jobs stored before the limit existed may carry a bigger timeout
    let timeout_ms = job.meta.timeout.min(app_state.worker_options.max_timeout);
    let schedule_id = job.schedule_id.clone();
    let mut req = hyper::Request::<Full<Bytes>>::try_from(job)?;
    add_delivery_headers(req.headers_mut(), job_id, retry, schedule_id.as_deref());
    let mut attrs = client_attrs(req.method(), req.uri());
    let span = tracing::info_span!(
        target: otel::TRACING_TARGET,
        "HTTP request",
        otel.name = %req.method(),
        otel.kind = ?SpanKind::Client,
        otel.status_code = Empty,
        http.request.method = %req.method(),
        url.full = %req.uri(),
        http.response.status_code = Empty,
        http.response.body.size = Empty,
        exception.message = Empty,
    );
    // The receiver continues the trace: traceparent/tracestate of this client span
    global::get_text_map_propagator(|propagator| {
        propagator.inject_context(&span.context(), &mut HeaderInjector(req.headers_mut()));
    });
    // Call: the timeout covers the whole exchange, including reading the body
    let call = async {
        // an IP address in the URL is not resolved: checked here (a name is checked when
        // resolved, see models::targets)
        if let Some(host) = req.uri().host()
            && let Err(detail) = app_state.worker_options.targets.check_host(host)
        {
            return Err(Error::BlockedTarget(format!("{host} {detail}")));
        }
        let response = app_state.client.request(req).await.map_err(|err| {
            match targets::blocked_target(&err) {
                Some(blocked) => Error::BlockedTarget(blocked.to_string()),
                None => Error::HyperClientError(err),
            }
        })?;
        debug!({ instance_id = app_state.instance_id, job_id }, "====> response={:?}", response);
        // StatusCode
        let status_code: hyper::StatusCode = response.status();
        let version: hyper::Version = response.version();
        // Headers
        let mut header_hashmap = HashMap::new();
        for (k, v) in response.headers() {
            if let Ok(val) = v.to_str() {
                header_hashmap.insert(k.to_string(), val.to_owned());
            }
        }
        // Body
        let collected = Limited::new(response.into_body(), MAX_RESPONSE_BODY)
            .collect()
            .await
            .map_err(|err| match err.downcast::<hyper::Error>() {
                Ok(err) => Error::HyperError(*err),
                Err(_) => Error::ResponseTooLarge,
            })?;
        Ok::<_, Error>((status_code, version, header_hashmap, collected.to_bytes()))
    };
    let start = Instant::now();
    // '?': timeout; the block yields the request/body result
    let exchange = async { time::timeout(Duration::from_millis(timeout_ms.into()), call).await? }
        .instrument(span.clone())
        .await;
    match &exchange {
        Ok((status_code, ..)) => {
            attrs.push(KeyValue::new(
                "http.response.status_code",
                i64::from(status_code.as_u16()),
            ));
            if status_code.is_client_error() || status_code.is_server_error() {
                attrs.push(KeyValue::new("error.type", status_code.as_str().to_owned()));
            }
        }
        Err(err) => attrs.push(KeyValue::new("error.type", error_type(err))),
    }
    metrics()
        .http_client_duration
        .record(start.elapsed().as_secs_f64(), &attrs);
    let (status_code, version, header_hashmap, bytes) = exchange.inspect_err(|err| {
        span.record("otel.status_code", "ERROR");
        span.record("exception.message", tracing::field::display(err));
    })?;
    // i64: unsigned values are exported as strings
    span.record("http.response.status_code", i64::from(status_code.as_u16()));
    span.record(
        "http.response.body.size",
        i64::try_from(bytes.len()).unwrap_or(i64::MAX),
    );
    // Result
    let job_result = JobResult::http(status_code, version, Some(header_hashmap), bytes);
    if status_code.is_server_error() {
        span.record("otel.status_code", "ERROR");
        return Err(Error::ServerError(Box::new(job_result)));
    }
    if status_code.is_client_error() {
        return Err(Error::ClientError(Box::new(job_result)));
    }
    Ok(job_result)
}

/// Lets the target recognize a redelivery (delivery is at least once): the same
/// `irisqo-job-id` with a higher `irisqo-retry`. A schedule run is a new job.
fn add_delivery_headers(
    headers: &mut hyper::HeaderMap,
    job_id: i64,
    retry: i32,
    schedule_id: Option<&str>,
) {
    headers.insert("irisqo-job-id", job_id.into());
    headers.insert("irisqo-retry", retry.into());
    if let Some(value) = schedule_id.and_then(|s| HeaderValue::from_str(s).ok()) {
        headers.insert("irisqo-schedule-id", value);
    }
}

/// `http.client.request.duration` attributes known before the request is sent.
fn client_attrs(method: &hyper::Method, uri: &hyper::Uri) -> Vec<KeyValue> {
    let port = uri
        .port_u16()
        .unwrap_or(if uri.scheme_str() == Some("https") {
            443
        } else {
            80
        });
    vec![
        KeyValue::new("http.request.method", method.as_str().to_owned()),
        KeyValue::new("server.address", uri.host().unwrap_or_default().to_owned()),
        KeyValue::new("server.port", i64::from(port)),
    ]
}

/// `error.type` of a failed exchange (no response).
fn error_type(err: &Error) -> &'static str {
    match err {
        Error::Timeout(_) => "timeout",
        Error::ResponseTooLarge => "response_too_large",
        Error::BlockedTarget(_) => "blocked_target",
        Error::HyperClientError(_) | Error::HyperError(_) | Error::IoError(_) => "connection",
        Error::JobNotFound(_)
        | Error::ResultNotFound(_)
        | Error::ScheduleNotFound(_)
        | Error::DbError(_)
        | Error::HttpError(_)
        | Error::InvalidParams(_)
        | Error::ServerError(_)
        | Error::ClientError(_)
        | Error::RetriesExceeded => "_OTHER",
    }
}

async fn processed_scheduled(app_state: &AppState, completion: &Completion) -> Result<(), Error> {
    let instance_id = &app_state.instance_id;
    let Completion {
        entry,
        schedule_id,
        result,
    } = completion;
    let entry = *entry;
    let job_id = entry.id;
    let next_at = schedule_next_at(app_state, schedule_id.as_deref()).await?;
    // Result and next schedule run are written atomically
    let mut tx = app_state.pool.begin().await?;
    let affected = results::processed(&mut *tx, entry, instance_id, result).await?;
    if affected == 0 {
        // The lock was lost (lock expired, instance expired, job deleted): nothing to complete or reschedule
        warn!({ instance_id, job_id }, "job is not locked by this attempt, skip");
        return Ok(());
    }
    if let Some(next_at) = next_at {
        // `None`: the schedule was deleted or deactivated meanwhile
        if let Some(next_id) =
            db::jobqueue::clone_schedule_at(&mut *tx, job_id, next_at, instance_id).await?
        {
            debug!({ instance_id, job_id, next_id, next_at = %next_at }, "==> clone and schedule");
        }
    }
    tx.commit().await?;
    Ok(())
}

/// When the schedule runs next. A database error is returned: the write is repeated.
async fn schedule_next_at(
    app_state: &AppState,
    schedule_id: Option<&str>,
) -> Result<Option<DateTime<Utc>>, Error> {
    let Some(schedule_id) = schedule_id else {
        return Ok(None);
    };
    let Some(row) = features::schedules::get_by_id(&app_state.pool, schedule_id).await? else {
        return Ok(None);
    };
    if row.paused {
        return Ok(None);
    }
    let Ok(schedule) = row.schedule.parse::<JobSchedule>().inspect_err(|err| {
        error!({ instance_id = app_state.instance_id, schedule_id }, "JobSchedule::parse error {:?}", err);
    }) else {
        return Ok(None);
    };
    Ok(schedule.next(Utc::now(), row.until))
}

/// Schedules a retry when the error is worth one; returns the final result otherwise.
async fn on_error(
    app_state: &AppState,
    entry: JobEntry,
    job_retry: JobRetry,
    err: Error,
) -> Option<JobResult> {
    let instance_id = &app_state.instance_id;
    let job_id = entry.id;
    let (retryable, retry_after) = match &err {
        Error::HyperError(_) | Error::HyperClientError(_) | Error::Timeout(_) => (true, None),
        Error::ServerError(res) | Error::ClientError(res) => (
            res.status_code().is_some_and(is_retryable_status),
            res.retry_after(Utc::now().timestamp()),
        ),
        Error::JobNotFound(_)
        | Error::ResultNotFound(_)
        | Error::ScheduleNotFound(_)
        | Error::DbError(_)
        | Error::IoError(_)
        | Error::HttpError(_)
        | Error::InvalidParams(_)
        | Error::ResponseTooLarge
        | Error::RetriesExceeded => {
            error!({ instance_id, job_id }, "====> error {:?}", err);
            (false, None)
        }
        Error::BlockedTarget(target) => {
            warn!({ instance_id, job_id }, "blocked target: {target}");
            (false, None)
        }
    };
    if !retryable {
        return Some(err.into());
    }
    match retry_or_fail(
        app_state,
        entry,
        job_retry,
        retry_after,
        &error_message(&err),
    )
    .await
    {
        Ok(()) => None,
        Err(Error::RetriesExceeded) => Some(err.into()),
        Err(retry_err) => {
            // Not the job's fault: keep it locked, the lock reaper releases it.
            error!({ instance_id, job_id }, "====> retry error {:?}, job error {:?}", retry_err, err);
            None
        }
    }
}

/// Why an attempt failed, for the history of the job.
fn error_message(err: &Error) -> String {
    match err {
        Error::ClientError(res) | Error::ServerError(res) => res.history_message(),
        Error::Timeout(_) => Some("timeout".into()),
        Error::JobNotFound(_)
        | Error::ResultNotFound(_)
        | Error::ScheduleNotFound(_)
        | Error::DbError(_)
        | Error::IoError(_)
        | Error::HttpError(_)
        | Error::HyperError(_)
        | Error::HyperClientError(_)
        | Error::InvalidParams(_)
        | Error::ResponseTooLarge
        | Error::BlockedTarget(_)
        | Error::RetriesExceeded => None,
    }
    .unwrap_or_else(|| err.to_string())
}

/// Backoff with jitter, but never sooner than the `Retry-After` of the response.
async fn retry_or_fail(
    app_state: &AppState,
    entry: JobEntry,
    job_retry: JobRetry,
    retry_after: Option<u32>,
    message: &str,
) -> Result<(), Error> {
    let instance_id = &app_state.instance_id;
    let JobEntry { id: job_id, retry } = entry;
    let attempt: u16 = retry.try_into().unwrap_or(u16::MAX);
    let delay = job_retry
        .next_retry_in(attempt)
        .ok_or(Error::RetriesExceeded)?;
    let delay = with_jitter(delay)
        .max(retry_after.unwrap_or(0))
        .min(MAX_RETRY_DELAY);
    let affected = if delay == 0 {
        debug!({ instance_id, job_id, retry }, "==> unlock");
        db::jobqueue::unlock(&app_state.pool, entry, instance_id, message).await?
    } else {
        let at = Utc::now() + chrono::Duration::seconds(i64::from(delay));
        debug!({ instance_id, job_id, retry, delay }, "==> retry");
        db::jobqueue::retry(&app_state.pool, entry, at, instance_id, message).await?
    };
    if affected == 0 {
        warn!({ instance_id, job_id }, "job is not locked by this attempt, skip retry");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    const FAST: [Duration; 2] = [Duration::from_millis(1); 2];

    #[tokio::test]
    async fn a_transient_error_is_repeated() {
        let calls = AtomicU32::new(0);
        let res = write_with_retry(FAST, || async {
            match calls.fetch_add(1, Ordering::SeqCst) {
                0..2 => Err(Error::DbError(sqlx::Error::PoolTimedOut)),
                _ => Ok(7),
            }
        })
        .await;
        assert_eq!(res.ok(), Some(7));
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn another_error_is_not_repeated() {
        let calls = AtomicU32::new(0);
        let res: Result<(), Error> = write_with_retry(FAST, || async {
            calls.fetch_add(1, Ordering::SeqCst);
            Err(Error::DbError(sqlx::Error::RowNotFound))
        })
        .await;
        assert!(res.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_write_is_repeated_only_so_often() {
        let calls = AtomicU32::new(0);
        let res: Result<(), Error> = write_with_retry(FAST, || async {
            calls.fetch_add(1, Ordering::SeqCst);
            Err(Error::DbError(sqlx::Error::PoolTimedOut))
        })
        .await;
        assert!(res.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }
}
