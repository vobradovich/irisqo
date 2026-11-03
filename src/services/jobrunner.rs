use crate::{
    db,
    features::{
        self,
        results::{self, JobResult, is_retryable_status},
        schedules::JobSchedule,
    },
    models::{
        AppState, Error, JobEntry, JobProtocol, JobRetry, JobRow, JobWithRetry, MAX_RETRY_DELAY,
        with_jitter,
    },
};
use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
#[allow(unused_imports)]
use opentelemetry::{global, trace::TraceContextExt};
use std::{collections::HashMap, time::Duration};
use tokio::time;
#[allow(unused_imports)]
use tracing::{debug, error, info, warn};

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
pub async fn job_execute(app_state: &AppState, job_with_retry: JobWithRetry) -> Option<Completion> {
    let instance_id = &app_state.instance_id;
    let JobWithRetry { job, retry } = job_with_retry;
    let entry = JobEntry { id: job.id, retry };
    let job_id = job.id;
    debug!({ instance_id, job_id, retry }, "==> run");
    let job_retry = job.meta.retry;
    let schedule_id = job.schedule_id.clone();
    let job_result = match job.meta.protocol {
        JobProtocol::None => Ok(JobResult::default()),
        JobProtocol::Http(_) => job_run_http(app_state, job).await,
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

/// Writes final results: one statement for all plain jobs, a transaction per scheduled
/// job (its result and the next run are written atomically).
pub async fn complete(app_state: &AppState, completions: Vec<Completion>) {
    let instance_id = &app_state.instance_id;
    let (scheduled, plain): (Vec<_>, Vec<_>) = completions
        .into_iter()
        .partition(|c| c.schedule_id.is_some());
    if !plain.is_empty() {
        let items: Vec<_> = plain.iter().map(|c| (c.entry, &c.result)).collect();
        let total = items.len();
        _ = results::processed_many(&app_state.pool, &items, instance_id)
            .await
            .inspect(|&affected| {
                if usize::try_from(affected).unwrap_or(usize::MAX) < total {
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
        _ = processed_scheduled(app_state, completion)
            .await
            .inspect_err(|err| error!({ instance_id, job_id }, "processed error {:?}", err));
    }
}

async fn job_run_http(app_state: &AppState, job: JobRow) -> Result<JobResult, Error> {
    let job_id = job.id;
    // jobs stored before the limit existed may carry a bigger timeout
    let timeout_ms = job.meta.timeout.min(app_state.worker_options.max_timeout);
    let req = hyper::Request::<Full<Bytes>>::try_from(job)?;
    // OpenTelemetry
    // let tracer = global::tracer("irisqo");
    // let span = opentelemetry::trace::Tracer::span_builder(&tracer, String::from("job_run_http"))
    //     .with_kind(opentelemetry::trace::SpanKind::Client)
    //     .start(&tracer);
    // let cx = opentelemetry::Context::current_with_span(span);

    // global::get_text_map_propagator(|propagator| {
    //     propagator.inject_context(&cx, &mut crate::otel::HeaderInjector(req.headers_mut()))
    // });
    // Call: the timeout covers the whole exchange, including reading the body
    let call = async {
        let response = app_state.client.request(req).await?;
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
    // first '?' - timeout
    // second '?' - request/body error
    let (status_code, version, header_hashmap, bytes) =
        time::timeout(Duration::from_millis(timeout_ms.into()), call).await??;
    // Result
    let job_result = JobResult::http(status_code, version, Some(header_hashmap), bytes);
    if status_code.is_server_error() {
        return Err(Error::ServerError(job_result));
    }
    if status_code.is_client_error() {
        return Err(Error::ClientError(job_result));
    }
    Ok(job_result)
}

async fn processed_scheduled(app_state: &AppState, completion: Completion) -> Result<(), Error> {
    let instance_id = &app_state.instance_id;
    let Completion {
        entry,
        schedule_id,
        result,
    } = completion;
    let job_id = entry.id;
    let next_at = schedule_next_at(app_state, schedule_id.as_deref()).await;
    // Result and next schedule run are written atomically
    let mut tx = app_state.pool.begin().await?;
    let affected = results::processed(&mut *tx, entry, instance_id, &result).await?;
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
            debug!({ instance_id, job_id, next_id, next_at }, "==> clone and schedule");
        }
    }
    tx.commit().await?;
    Ok(())
}

async fn schedule_next_at(app_state: &AppState, schedule_id: Option<&str>) -> Option<i64> {
    let schedule_id = schedule_id?;

    let row = features::schedules::get_by_id(&app_state.pool, schedule_id).await.map_err(|err| {
        error!({ instance_id = app_state.instance_id, schedule_id }, "schedules::get_by_id error {:?}", err);
    }).ok()??;
    if row.inactive {
        return None;
    }
    let schedule = row.schedule.parse::<JobSchedule>().map_err(|err| {
        error!({ instance_id = app_state.instance_id, schedule_id }, "JobSchedule::parse error {:?}", err);
    }).ok()?;
    schedule.next(JobSchedule::now_secs(), row.until)
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
            res.retry_after(JobSchedule::now_secs()),
        ),
        Error::JobNotFound(_)
        | Error::DbError(_)
        | Error::IoError(_)
        | Error::HttpError(_)
        | Error::InvalidUrl
        | Error::InvalidParams(_)
        | Error::ResponseTooLarge
        | Error::RetriesExceeded => {
            error!({ instance_id, job_id }, "====> error {:?}", err);
            (false, None)
        }
    };
    if !retryable {
        return Some(err.into());
    }
    match retry_or_fail(app_state, entry, job_retry, retry_after).await {
        Ok(()) => None,
        Err(Error::RetriesExceeded) => Some(err.into()),
        Err(retry_err) => {
            // Not the job's fault: keep it locked, the lock reaper releases it.
            error!({ instance_id, job_id }, "====> retry error {:?}, job error {:?}", retry_err, err);
            None
        }
    }
}

/// Backoff with jitter, but never sooner than the `Retry-After` of the response.
async fn retry_or_fail(
    app_state: &AppState,
    entry: JobEntry,
    job_retry: JobRetry,
    retry_after: Option<u32>,
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
        db::jobqueue::unlock(&app_state.pool, entry, instance_id).await?
    } else {
        let at = JobSchedule::now_secs() + i64::from(delay);
        debug!({ instance_id, job_id, retry, delay }, "==> retry");
        db::jobqueue::retry(&app_state.pool, entry, at, instance_id).await?
    };
    if affected == 0 {
        warn!({ instance_id, job_id }, "job is not locked by this attempt, skip retry");
    }
    Ok(())
}
