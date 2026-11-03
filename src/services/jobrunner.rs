use crate::{
    db,
    features::{
        self,
        results::{self, JobResult},
        schedules::JobSchedule,
    },
    models::{AppState, Error, JobEntry, JobMeta, JobProtocol, JobRow, JobWithRetry},
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

pub async fn job_run(app_state: &AppState, job_with_retry: JobWithRetry) {
    let instance_id = &app_state.instance_id;
    let job_id = job_with_retry.job.id;
    let retry = job_with_retry.retry;
    _ = job_run_with_error(app_state, job_with_retry)
        .await
        .map_err(|err| {
            error!({ instance_id, job_id, retry }, "run error {:?}", err);
        });
}

async fn job_run_with_error(
    app_state: &AppState,
    job_with_retry: JobWithRetry,
) -> Result<(), Error> {
    let instance_id = &app_state.instance_id;
    let JobWithRetry { job, retry } = job_with_retry;
    let job_id = job.id;
    debug!({ instance_id, job_id, retry }, "==> run");
    let meta = job.meta.clone();
    let schedule_id = job.schedule_id.clone();
    let job_result = match meta.protocol {
        JobProtocol::None => Ok(JobResult::default()),
        JobProtocol::Http(_) => job_run_http(app_state, job).await,
    };
    match job_result {
        Ok(result) => {
            processed(app_state, job_id, schedule_id.as_deref(), result).await?;
        }
        Err(err) => {
            if let Some(res) = on_error(app_state, JobEntry { id: job_id, retry }, meta, err).await
            {
                warn!({ instance_id, job_id }, "====> job failed={:?}", &res.meta);
                processed(app_state, job_id, schedule_id.as_deref(), res).await?;
            }
        }
    }
    Ok(())
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

async fn processed(
    app_state: &AppState,
    job_id: i64,
    schedule_id: Option<&str>,
    result: JobResult,
) -> Result<(), Error> {
    let instance_id = &app_state.instance_id;
    let next_at = schedule_next_at(app_state, schedule_id).await;
    // Result and next schedule run are written atomically
    let mut tx = app_state.pool.begin().await?;
    let affected = results::processed(&mut *tx, job_id, instance_id, result).await?;
    if affected == 0 {
        // The lock was lost (instance expired, job deleted): nothing to complete or reschedule
        warn!({ instance_id, job_id }, "job is not locked by this instance, skip");
        return Ok(());
    }
    if let Some(next_at) = next_at {
        match db::jobqueue::clone_schedule_at(&mut *tx, job_id, next_at, instance_id).await? {
            Some(next_id) => {
                debug!({ instance_id, job_id, next_id, next_at }, "==> clone and schedule");
            }
            None => {
                debug!({ instance_id, job_id }, "==> schedule deleted or inactive, stop");
            }
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

async fn on_error(
    app_state: &AppState,
    entry: JobEntry,
    meta: JobMeta,
    err: Error,
) -> Option<JobResult> {
    let job_id = entry.id;
    match err {
        Error::HyperError(_)
        | Error::HyperClientError(_)
        | Error::Timeout(_)
        | Error::ServerError(_)
        | Error::ClientError(_) => match retry_or_fail(app_state, entry, meta).await {
            Ok(()) => None,
            Err(Error::RetriesExceeded) => Some(err.into()),
            Err(retry_err) => {
                // Not the job's fault: keep it locked, it is released when this instance
                // is killed (or by the lease reaper) and runs again.
                error!({ instance_id = app_state.instance_id, job_id }, "====> retry error {:?}, job error {:?}", retry_err, err);
                None
            }
        },
        Error::JobNotFound(_)
        | Error::DbError(_)
        | Error::IoError(_)
        | Error::HttpError(_)
        | Error::InvalidUrl
        | Error::InvalidParams(_)
        | Error::ResponseTooLarge
        | Error::RetriesExceeded => {
            error!({ instance_id = app_state.instance_id, job_id }, "====> error {:?}", err);
            let job_result: JobResult = err.into();
            Some(job_result)
        }
    }
}

async fn retry_or_fail(app_state: &AppState, entry: JobEntry, meta: JobMeta) -> Result<(), Error> {
    let instance_id = &app_state.instance_id;
    let JobEntry { id: job_id, retry } = entry;
    let retry: u16 = retry.try_into().unwrap_or(u16::MAX);
    match meta.retry.next_retry_in(retry) {
        None => Err(Error::RetriesExceeded),
        Some(0) => {
            let affected = db::jobqueue::unlock(&app_state.pool, job_id, instance_id).await?;
            if affected == 0 {
                warn!({ instance_id, job_id }, "job is not locked by this instance, skip unlock");
            }
            debug!({ instance_id, job_id, retry }, "==> unlock");
            Ok(())
        }
        Some(delay) => {
            let now_secs = JobSchedule::now_secs();
            let at = now_secs + i64::from(delay);
            let affected = db::jobqueue::retry(&app_state.pool, job_id, at, instance_id).await?;
            if affected == 0 {
                warn!({ instance_id, job_id }, "job is not locked by this instance, skip retry");
            }
            debug!({ instance_id, job_id, retry }, "==> retry");
            Ok(())
        }
    }
}
