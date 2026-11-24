use crate::models::{CallbackMode, SigningKeys};
use crate::{
    db,
    features::{
        self,
        callbacks::JobOutcome,
        results::{self, JobResult, is_retryable_status},
        schedules::JobSchedule,
    },
    models::{
        AppState, Error, JobCreate, JobEntry, JobMeta, JobRetry, JobRow, JobSink, JobWithAttempt,
        targets,
    },
    otel::{self, HeaderInjector, metrics::metrics},
};
use bytes::Bytes;
use chrono::{DateTime, Utc};
use http_body_util::BodyExt;
use hyper::body::Body;
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

/// Final result of a job attempt, to be written to `processed`.
#[derive(Debug)]
pub struct Completion {
    pub entry: JobEntry,
    pub schedule_id: Option<String>,
    pub result: JobResult,
    /// The callback this outcome makes, built with the outcome and written with it.
    pub callback: Option<Box<JobCreate>>,
    /// Writing it makes a job claimable: it frees a place of its flow or enqueues a callback.
    pub wakes: bool,
}

impl Completion {
    /// What `processed_many` writes. A fn, not a closure: the completer's future stays `Send`.
    fn row(&self) -> (JobEntry, &JobResult, Option<&JobCreate>) {
        (self.entry, &self.result, self.callback.as_deref())
    }
}

/// Another attempt of a job, to be written back to the queue.
#[derive(Debug)]
pub struct Retry {
    pub entry: JobEntry,
    /// `None`: at once (`enqueued`), otherwise `scheduled` at this time.
    pub at: Option<DateTime<Utc>>,
    pub message: String,
    /// Writing it frees a place of the job's flow.
    pub wakes: bool,
}

/// How a job attempt ends; [`complete`] writes it.
#[derive(Debug)]
pub enum Outcome {
    Done(Completion),
    Retry(Retry),
}

impl Outcome {
    #[must_use]
    pub fn wakes(&self) -> bool {
        match self {
            Self::Done(completion) => completion.wakes,
            Self::Retry(retry) => retry.wakes,
        }
    }
}

/// Runs a job and writes its outcome right away.
pub async fn job_run(app_state: &AppState, job_with_attempt: JobWithAttempt) {
    let outcome = job_execute(app_state, job_with_attempt).await;
    complete(app_state, vec![outcome]).await;
}

/// Runs a job attempt and returns its outcome, a final result or a retry, for [`complete`]
/// (so that the outcomes are written in batches, by one writer).
///
/// The attempt is a `job.run` span, a child of the request that created the job
/// (`meta.traceparent`), so retries are siblings. A schedule run instead starts its
/// own trace with a link to that request: a schedule would grow one trace forever.
pub async fn job_execute(app_state: &AppState, job_with_attempt: JobWithAttempt) -> Outcome {
    let meta = &job_with_attempt.job.meta;
    let span = tracing::info_span!(
        target: otel::TRACING_TARGET,
        "job.run",
        otel.name = "job.run",
        otel.kind = ?SpanKind::Consumer,
        otel.status_code = Empty,
        job.id = job_with_attempt.job.id,
        job.attempt = job_with_attempt.attempt,
        job.schedule_id = job_with_attempt.job.schedule_id.as_deref(),
        job.outcome = Empty,
    );
    let request =
        otel::context_from_trace_headers(meta.traceparent.as_deref(), meta.tracestate.as_deref());
    let parent = if job_with_attempt.job.schedule_id.is_some() {
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
    let metrics = metrics();
    metrics.jobs_in_flight.add(1, &[]);
    let start = Instant::now();
    let outcome = job_attempt(app_state, job_with_attempt)
        .instrument(span.clone())
        .await;
    metrics.jobs_in_flight.add(-1, &[]);
    let status = match &outcome {
        Outcome::Done(completion) => completion.result.processed_status(),
        Outcome::Retry(_) => "retry",
    };
    metrics.job_attempt_duration.record(
        start.elapsed().as_secs_f64(),
        &[KeyValue::new("job.outcome", status)],
    );
    span.record("job.outcome", status);
    span.record(
        "otel.status_code",
        if status == "completed" { "OK" } else { "ERROR" },
    );
    outcome
}

/// Runs inside the span of the attempt: its trace and span are kept in the result.
async fn job_attempt(app_state: &AppState, job_with_attempt: JobWithAttempt) -> Outcome {
    let instance_id = &app_state.instance_id;
    let JobWithAttempt { mut job, attempt } = job_with_attempt;
    let entry = JobEntry {
        id: job.id,
        attempt,
    };
    let job_id = job.id;
    debug!({ instance_id, job_id, attempt }, "==> run");
    let job_result = match job.meta.sink {
        JobSink::Noop => Ok(JobResult::default()),
        JobSink::Http(_) => job_run_http(app_state, &mut job, attempt).await,
    };
    let mut result = match job_result {
        Ok(result) => result,
        Err(err) => match retry_or_error(instance_id, entry, &job.meta, err) {
            Ok(retry) => return Outcome::Retry(retry),
            Err(err) => {
                let result = JobResult::from(err);
                warn!({ instance_id, job_id }, "====> job failed={:?}", &result.meta);
                result
            }
        },
    };
    let span = tracing::Span::current();
    result.traced(otel::span_ids(&span));
    // the callback goes on the attempt's trace and sampling flag, once the result is complete
    let callback = job.meta.callback.as_ref().and_then(|_| {
        let trace = otel::span_traceparent(&span);
        JobOutcome::of(&job, attempt, &result, trace)
            .callback()
            .map(Box::new)
    });
    Outcome::Done(Completion {
        entry,
        wakes: job.meta.flow_key.is_some() || callback.is_some(),
        schedule_id: job.schedule_id,
        result,
        callback,
    })
}

/// Pauses between repeats of a failed write: 250 ms doubling, 30 s in all (the lease is renewed
/// meanwhile).
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

/// Writes the outcomes: one statement for all retries, one for the final results of all plain
/// jobs, a transaction per scheduled job (its result and the next run are written atomically).
pub async fn complete(app_state: &AppState, outcomes: Vec<Outcome>) {
    let instance_id = &app_state.instance_id;
    let (mut plain, mut scheduled) = (Vec::with_capacity(outcomes.len()), Vec::new());
    let mut retries = db::jobqueue::Retries::default();
    for outcome in outcomes {
        match outcome {
            Outcome::Done(completion) if completion.schedule_id.is_some() => {
                scheduled.push(completion);
            }
            Outcome::Done(completion) => plain.push(completion),
            Outcome::Retry(retry) => retries.push(retry.entry, retry.at, retry.message),
        }
    }
    if !retries.is_empty() {
        let total = retries.len() as u64;
        _ = write_with_retry(write_pauses(), || {
            db::jobqueue::retry_many(&app_state.pool, instance_id, &retries)
        })
        .await
        .inspect(|&affected| {
            if affected < total {
                warn!({ instance_id, total, affected }, "some jobs are not locked by this attempt, skip retry");
            }
        })
        .inspect_err(|err| error!({ instance_id, total }, "db::jobqueue::retry_many error {:?}", err));
    }
    if !plain.is_empty() {
        let total = plain.len() as u64;
        _ = write_with_retry(write_pauses(), || {
            results::processed_many(&app_state.pool, plain.iter().map(Completion::row), instance_id)
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
    for completion in &scheduled {
        let job_id = completion.entry.id;
        _ = write_with_retry(write_pauses(), || {
            processed_scheduled(app_state, completion)
        })
        .await
        .inspect_err(|err| error!({ instance_id, job_id }, "processed error {:?}", err));
    }
    // written or given up: a job still claimed is released when its lease ends
    let done = plain.iter().chain(&scheduled).map(|c| c.entry);
    app_state.held.remove(done.chain(retries.entries()));
}

/// Calls the target of the job. The body of the row is taken out: the one buffer is sent, and
/// signed when the job is a callback.
async fn job_run_http(
    app_state: &AppState,
    job: &mut JobRow,
    attempt: i32,
) -> Result<JobResult, Error> {
    let job_id = job.id;
    // jobs stored before the limit existed may carry a bigger timeout
    let options = &app_state.options.worker;
    let timeout_ms = job
        .meta
        .timeout
        .unwrap_or(options.timeout)
        .min(options.max_timeout);
    let delivery = job.meta.callback_as;
    let (mut req, body) = job.take_request()?;
    // a webhook has the headers of the standard and none of ours
    if delivery != Some(CallbackMode::Webhook) {
        add_delivery_headers(
            req.headers_mut(),
            job_id,
            attempt,
            job.schedule_id.as_deref(),
        );
    }
    // the signed text has the body as it is sent
    if let (Some(delivery), Some(key)) = (delivery, job.key.as_deref()) {
        add_callback_headers(
            req.headers_mut(),
            delivery,
            key,
            &body,
            options.signing_keys.as_ref(),
        );
    }
    let mut attrs = client_attrs(req.method(), req.uri());
    let span = tracing::info_span!(
        target: otel::TRACING_TARGET,
        "HTTP request",
        otel.name = %req.method(),
        otel.kind = ?SpanKind::Client,
        otel.status_code = Empty,
        http.request.method = %req.method(),
        url.scheme = req.uri().scheme_str().unwrap_or_default(),
        server.address = req.uri().host().unwrap_or_default(),
        server.port = req.uri().port_u16(),
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
        // an IP in the URL is not resolved, so it is checked here (names: models::targets)
        if let Some(host) = req.uri().host()
            && let Err(detail) = app_state.targets.check_host(host)
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
        let body = read_limited(response.into_body(), options.max_response_size).await?;
        Ok::<_, Error>((status_code, version, header_hashmap, body))
    };
    let start = Instant::now();
    // '?': timeout; the block yields the request/body result
    let exchange = async { time::timeout(Duration::from_millis(timeout_ms.into()), call).await? }
        .instrument(span.clone())
        .await;
    match &exchange {
        Ok((status_code, _, _, body)) => {
            attrs.push(KeyValue::new(
                "http.response.status_code",
                i64::from(status_code.as_u16()),
            ));
            if !status_code.is_success() {
                attrs.push(KeyValue::new("error.type", status_code.as_str().to_owned()));
            } else if matches!(body, ResponseBody::TooLarge { .. }) {
                attrs.push(KeyValue::new("error.type", "response_too_large"));
            }
        }
        Err(err) => attrs.push(KeyValue::new("error.type", error_type(err))),
    }
    metrics()
        .http_client_duration
        .record(start.elapsed().as_secs_f64(), &attrs);
    let (status_code, version, header_hashmap, body) = exchange.inspect_err(|err| {
        span.record("otel.status_code", "ERROR");
        span.record("exception.message", tracing::field::display(err));
    })?;
    // i64: unsigned values are exported as strings
    span.record("http.response.status_code", i64::from(status_code.as_u16()));
    // a body over the limit is not kept: the result has the status and the headers only
    let (bytes, too_large) = match body {
        ResponseBody::Whole(bytes) => {
            span.record(
                "http.response.body.size",
                i64::try_from(bytes.len()).unwrap_or(i64::MAX),
            );
            (bytes, None)
        }
        ResponseBody::TooLarge { size } => (Bytes::new(), Some(size)),
    };
    // Result
    let job_result = JobResult::http(status_code, version, Some(header_hashmap), bytes);
    if status_code.is_server_error() {
        span.record("otel.status_code", "ERROR");
        return Err(Error::ServerError(Box::new(job_result)));
    }
    // a 3xx is not followed and not a success either: it is not worth another attempt
    if !status_code.is_success() {
        return Err(Error::ClientError(Box::new(job_result)));
    }
    // a success whose body is not here is not a success of the job
    if let Some(size) = too_large {
        span.record("otel.status_code", "ERROR");
        let max_size = options.max_response_size;
        let error = match size {
            Some(size) => format!("response too large: {size} bytes, the limit is {max_size}"),
            None => format!("response too large: over the limit of {max_size} bytes"),
        };
        // a 2xx is never retried: it ends as a failure of the job
        return Err(Error::ClientError(Box::new(job_result.failed_with(error))));
    }
    Ok(job_result)
}

/// A response body as far as `--max-response-size` lets it be read.
enum ResponseBody {
    Whole(Bytes),
    /// Longer than the limit: not read to its end; `size` is the length it declared, if it did.
    TooLarge {
        size: Option<usize>,
    },
}

/// Reads a response body of `limit` bytes at most: it is too large by its declared length, else
/// when it has gone over. A body that came in one piece is not copied.
async fn read_limited(
    mut body: hyper::body::Incoming,
    limit: usize,
) -> Result<ResponseBody, Error> {
    let declared = body
        .size_hint()
        .exact()
        .and_then(|n| usize::try_from(n).ok());
    if declared.is_some_and(|size| size > limit) {
        return Ok(ResponseBody::TooLarge { size: declared });
    }
    let mut chunks: Vec<Bytes> = Vec::new();
    let mut size = 0;
    while let Some(frame) = body.frame().await {
        let Ok(data) = frame.map_err(Error::HyperError)?.into_data() else {
            continue;
        };
        size += data.len();
        if size > limit {
            return Ok(ResponseBody::TooLarge { size: declared });
        }
        if !data.is_empty() {
            chunks.push(data);
        }
    }
    Ok(ResponseBody::Whole(match chunks.as_mut_slice() {
        [] => Bytes::new(),
        [whole] => std::mem::take(whole),
        many => many.concat().into(),
    }))
}

/// Lets the target recognize a redelivery (delivery is at least once): the same
/// `irisqo-job-id` with a higher `irisqo-attempt`. A schedule run is a new job.
fn add_delivery_headers(
    headers: &mut hyper::HeaderMap,
    job_id: i64,
    attempt: i32,
    schedule_id: Option<&str>,
) {
    headers.insert("irisqo-job-id", job_id.into());
    headers.insert("irisqo-attempt", attempt.into());
    if let Some(value) = schedule_id.and_then(|s| HeaderValue::from_str(s).ok()) {
        headers.insert("irisqo-schedule-id", value);
    }
}

/// The headers a callback gets when it is sent: the key of its job, the time of this delivery
/// and, when there are signing keys, the signature of `{key}.{timestamp}.{body}`. A webhook
/// names them as the standard does, a relay as `Irisqo-*`.
fn add_callback_headers(
    headers: &mut hyper::HeaderMap,
    delivery: CallbackMode,
    key: &str,
    body: &[u8],
    signing_keys: Option<&SigningKeys>,
) {
    let (key_name, time_name, signature_name) = match delivery {
        CallbackMode::Webhook => ("webhook-id", "webhook-timestamp", "webhook-signature"),
        CallbackMode::Relay => ("irisqo-job-key", "irisqo-timestamp", "irisqo-signature"),
    };
    let timestamp = Utc::now().timestamp();
    if let Ok(value) = HeaderValue::from_str(key) {
        headers.insert(key_name, value);
    }
    headers.insert(time_name, timestamp.into());
    if let Some(keys) = signing_keys
        && let Ok(value) = HeaderValue::from_str(&keys.sign(key, timestamp, body))
    {
        headers.insert(signature_name, value);
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
        Error::BlockedTarget(_) => "blocked_target",
        Error::HyperClientError(_) | Error::HyperError(_) | Error::IoError(_) => "connection",
        Error::JobNotFound(_)
        | Error::ResultNotFound(_)
        | Error::ScheduleNotFound(_)
        | Error::FlowNotFound(_)
        | Error::DbError(_)
        | Error::HttpError(_)
        | Error::InvalidParams(_)
        | Error::ServerError(_)
        | Error::ClientError(_) => "_OTHER",
    }
}

async fn processed_scheduled(app_state: &AppState, completion: &Completion) -> Result<(), Error> {
    let instance_id = &app_state.instance_id;
    let Completion {
        entry,
        schedule_id,
        result,
        callback,
        ..
    } = completion;
    let entry = *entry;
    let job_id = entry.id;
    let next_at = schedule_next_at(app_state, schedule_id.as_deref()).await?;
    // Result and next schedule run are written atomically
    let mut tx = app_state.pool.begin().await?;
    let affected =
        results::processed(&mut *tx, entry, instance_id, result, callback.as_deref()).await?;
    if affected == 0 {
        // The lock was lost (lock expired, instance expired, job deleted): nothing to complete or reschedule
        warn!({ instance_id, job_id }, "job is not locked by this attempt, skip");
        return Ok(());
    }
    if let (Some(schedule_id), Some(next_at)) = (schedule_id.as_deref(), next_at) {
        // `None`: the schedule was deleted or deactivated meanwhile
        if let Some(next_id) = features::schedules::clone_schedule_at(
            &mut *tx,
            schedule_id,
            job_id,
            next_at,
            instance_id,
        )
        .await?
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
    let Ok(schedule) = JobSchedule::from_row(&row.rule, &row.tz, row.until).inspect_err(|err| {
        error!({ instance_id = app_state.instance_id, schedule_id }, "JobSchedule::parse error {:?}", err);
    }) else {
        return Ok(None);
    };
    // not before the current run's time: an early run must not schedule the next at its own time
    let after = row.next_at.map_or_else(Utc::now, |at| at.max(Utc::now()));
    Ok(schedule.next(after))
}

/// Another attempt when the error is worth one and the policy of `meta` allows it;
/// otherwise the error back, the final result of the job.
fn retry_or_error(
    instance_id: &str,
    entry: JobEntry,
    meta: &JobMeta,
    err: Error,
) -> Result<Retry, Error> {
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
        | Error::FlowNotFound(_)
        | Error::DbError(_)
        | Error::IoError(_)
        | Error::HttpError(_)
        | Error::InvalidParams(_) => {
            error!({ instance_id, job_id }, "====> error {:?}", err);
            (false, None)
        }
        Error::BlockedTarget(target) => {
            warn!({ instance_id, job_id }, "blocked target: {target}");
            (false, None)
        }
    };
    if !retryable {
        return Err(err);
    }
    let Some(delay) = retry_delay(entry.attempt, meta.retry, retry_after) else {
        return Err(err);
    };
    Ok(Retry {
        entry,
        at: (delay > 0).then(|| Utc::now() + chrono::Duration::seconds(i64::from(delay))),
        message: error_message(&err),
        wakes: meta.flow_key.is_some(),
    })
}

/// Why an attempt failed, for the history of the job.
fn error_message(err: &Error) -> String {
    match err {
        Error::ClientError(res) | Error::ServerError(res) => res.history_message(),
        Error::Timeout(_) => Some("timeout".into()),
        Error::JobNotFound(_)
        | Error::ResultNotFound(_)
        | Error::ScheduleNotFound(_)
        | Error::FlowNotFound(_)
        | Error::DbError(_)
        | Error::IoError(_)
        | Error::HttpError(_)
        | Error::HyperError(_)
        | Error::HyperClientError(_)
        | Error::InvalidParams(_)
        | Error::BlockedTarget(_) => None,
    }
    .unwrap_or_else(|| err.to_string())
}

/// Seconds until the next attempt: backoff with jitter, but never sooner than the
/// `Retry-After` of the response. `None` when the retries are exhausted.
fn retry_delay(attempt: i32, job_retry: Option<JobRetry>, retry_after: Option<u32>) -> Option<u32> {
    let attempt: u16 = attempt.try_into().unwrap_or(u16::MAX);
    let job_retry = job_retry?;
    let delay = job_retry.jittered_delay(attempt)?;
    // up to the longest delay of the policy
    Some(delay.max(retry_after.unwrap_or(0).min(job_retry.max_delay())))
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
