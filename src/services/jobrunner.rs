use crate::{
    db,
    features::{
        self,
        results::{self, JobResult},
        schedules::JobSchedule,
    },
    models::{AppState, Error, JobEntry, JobMeta, JobProtocol, JobRow},
};
use bytes::Bytes;
use http_body_util::Full;
use std::{collections::HashMap, time::Duration};
use tokio::time;
#[allow(unused_imports)]
use tracing::{debug, error, info, warn};

pub async fn job_run(app_state: &AppState, entry: JobEntry) {
    let res = job_run_with_error(app_state, entry).await;
    if let Err(err) = res {
        let JobEntry { id: job_id, retry } = entry;
        error!({ instance_id = app_state.instance_id, job_id, retry }, "run error {:?}", err);
    }
}

async fn job_run_with_error(app_state: &AppState, entry: JobEntry) -> Result<(), Error> {
    let JobEntry { id: job_id, retry } = entry;
    debug!({ instance_id = app_state.instance_id, job_id, retry }, "==> run");
    let job = db::jobqueue::get_by_id(&app_state.pool, job_id)
        .await?
        .ok_or(Error::JobNotFound(job_id))?;
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
            if let Some(res) = on_error(app_state, entry, meta, err).await {
                warn!({ instance_id = app_state.instance_id, job_id }, "====> processed={:?}", &res.meta);
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
    let collected = http_body_util::BodyExt::collect(response.into_body()).await?;
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
        let next_id = db::jobqueue::clone_at(&app_state.pool, job_id, next_at, &app_state.instance_id).await?;
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
    let next_at = schedule.next(JobSchedule::now_secs(), row.until);
    next_at
}

async fn on_error(
    app_state: &AppState,
    entry: JobEntry,
    meta: JobMeta,
    err: Error,
) -> Option<JobResult> {
    let job_id = entry.id;
    match err {
        Error::HyperError(_) | Error::Timeout(_) | Error::ServerError(_) => {
            info!({ instance_id = app_state.instance_id, job_id }, "====> error {:?}", err);
            let res = retry_or_fail(app_state, entry, meta).await;
            if let Ok(_) = res {
                return None;
            }
            info!({ instance_id = app_state.instance_id, job_id }, "====> retry_or_fail {:?}", res.err());
            let job_result: JobResult = err.into();
            Some(job_result)
        }
        Error::ClientError(res) => {
            info!({ instance_id = app_state.instance_id, job_id }, "====> error {:?}", res.meta);
            Some(res)
        }
        _ => {
            error!({ instance_id = app_state.instance_id, job_id }, "====> error {:?}", err);
            let job_result: JobResult = err.into();
            Some(job_result)
        }
    }
}

async fn retry_or_fail(app_state: &AppState, entry: JobEntry, meta: JobMeta) -> Result<(), Error> {
    let JobEntry { id: job_id, retry } = entry;
    let retry: u16 = retry.try_into().unwrap_or(u16::MAX);
    match meta.retry.next_retry_in(retry) {
        None => Err(Error::RetriesExceeded),
        Some(0) => {
            db::jobqueue::unlock(&app_state.pool, job_id, &app_state.instance_id).await?;
            Ok(())
        }
        Some(delay) => {
            let now_secs = JobSchedule::now_secs();
            let at = now_secs + i64::from(delay);
            db::jobqueue::retry(&app_state.pool, job_id, at).await?;
            debug!({ instance_id = app_state.instance_id, job_id, retry }, "==> retry");
            Ok(())
        }
    }
}
