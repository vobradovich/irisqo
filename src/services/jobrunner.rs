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
use http_body_util::{BodyExt, Full};
#[allow(unused_imports)]
use opentelemetry::{global, trace::TraceContextExt};
use std::{collections::HashMap, time::Duration};
use tokio::time;
#[allow(unused_imports)]
use tracing::{debug, error, info, warn};

#[allow(dead_code)]
pub async fn job_get_and_run(app_state: &AppState, entry: JobEntry) {
    let instance_id = &app_state.instance_id;
    let JobEntry { id: job_id, retry } = entry;
    let job = db::jobqueue::get_by_id(&app_state.pool, job_id)
        .await
        .and_then(|o| o.ok_or(Error::JobNotFound(job_id)))
        .map_err(|err| {
            error!({ instance_id, job_id, retry }, "db::jobqueue::get_by_id error {:?}", err);
        });
    if let Ok(job) = job {
        let job_with_retry = JobWithRetry { job, retry };
        job_run(app_state, job_with_retry).await;
    }
}

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
    let timeout_ms = job.meta.timeout;
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
    // Call
    let future = app_state.client.request(req);
    // first '?' - timeout
    // second '?' - HyperError
    let response = time::timeout(Duration::from_millis(timeout_ms.into()), future).await??;
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
    let collected = response.into_body().collect().await?;
    let bytes = collected.to_bytes();
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
    results::processed(&app_state.pool, job_id, result).await?;
    let next_at = schedule_next_at(app_state, schedule_id).await;
    if let Some(next_at) = next_at {
        let next_id = db::jobqueue::clone_schedule_at(
            &app_state.pool,
            job_id,
            next_at,
            &app_state.instance_id,
        )
        .await?;
        debug!({ instance_id = app_state.instance_id, job_id, next_id, next_at }, "==> clone and schedule");
    }
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
        | Error::Timeout(_)
        | Error::ServerError(_)
        | Error::ClientError(_) => {
            let res = retry_or_fail(app_state, entry, meta).await;
            if res.is_ok() {
                return None;
            }
            let job_result: JobResult = err.into();
            Some(job_result)
        }
        _ => {
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
            db::jobqueue::unlock(&app_state.pool, job_id, instance_id).await?;
            debug!({ instance_id, job_id, retry }, "==> unlock");
            Ok(())
        }
        Some(delay) => {
            let now_secs = JobSchedule::now_secs();
            let at = now_secs + i64::from(delay);
            db::jobqueue::retry(&app_state.pool, job_id, at).await?;
            debug!({ instance_id, job_id, retry }, "==> retry");
            Ok(())
        }
    }
}
