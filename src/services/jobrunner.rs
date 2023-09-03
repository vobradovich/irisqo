use crate::{
    db,
    models::{
        AppState, Error, HttpResponseMeta, JobEntry, JobMeta, JobProtocol, JobResult,
        JobResultMeta, JobResultType, JobRow,
    },
};
use std::{
    collections::HashMap,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
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
    let meta = job.meta.0.clone();
    let job_result = match meta.protocol {
        JobProtocol::None => Ok(JobResult::default()),
        JobProtocol::Http(_) => job_run_http(app_state, job).await,
    };
    match job_result {
        Ok(result) => {
            db::jobqueue::complete(&app_state.pool, job_id, result).await?;
        }
        Err(err) => {
            on_error(app_state, entry, meta, err).await?;
        }
    }
    Ok(())
}

async fn job_run_http(app_state: &AppState, job: JobRow) -> Result<JobResult, Error> {
    let job_id = job.id;
    let timeout_ms = job.meta.timeout;
    let req = hyper::Request::<hyper::Body>::try_from(job)?;
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
    let bytes = hyper::body::to_bytes(response.into_body()).await?;
    // Result
    let job_result = JobResult {
        meta: JobResultMeta {
            result: JobResultType::Http(HttpResponseMeta {
                status_code,
                version,
            }),
        },
        headers: Some(header_hashmap),
        body: bytes,
    };
    if status_code.is_server_error() {
        return Err(Error::ServerError(job_result));
    }
    if status_code.is_client_error() {
        return Err(Error::ClientError(job_result));
    }
    Ok(job_result)
}

async fn on_error(
    app_state: &AppState,
    entry: JobEntry,
    meta: JobMeta,
    err: Error,
) -> Result<(), Error> {
    let job_id = entry.id;
    match err {
        Error::HyperError(_) | Error::Timeout(_) => {
            info!({ instance_id = app_state.instance_id, job_id }, "====> error {:?}", err);
            retry_or_fail(app_state, entry, meta, err.into()).await?;
        }
        Error::ServerError(res) => {
            info!({ instance_id = app_state.instance_id, job_id }, "====> error {:?}", res.meta);
            retry_or_fail(app_state, entry, meta, res).await?;
        }
        Error::ClientError(res) => {
            info!({ instance_id = app_state.instance_id, job_id }, "====> error {:?}", res.meta);
            db::jobqueue::fail(&app_state.pool, job_id, res).await?;
        }
        _ => {
            error!({ instance_id = app_state.instance_id, job_id }, "====> error {:?}", err);
            db::jobqueue::fail(&app_state.pool, job_id, err.into()).await?;
        }
    };
    Ok(())
}

async fn retry_or_fail(
    app_state: &AppState,
    entry: JobEntry,
    meta: JobMeta,
    job_result: JobResult,
) -> Result<(), Error> {
    let JobEntry { id: job_id, retry } = entry;
    let retry: u16 = retry.try_into().unwrap_or(0);
    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    match meta.retry.next_retry_in(retry) {
        None => {
            db::jobqueue::fail(&app_state.pool, job_id, job_result).await?;
        }
        Some(0) => {
            db::jobqueue::unlock(&app_state.pool, job_id).await?;
        }
        Some(delay) => {
            let at = now_secs + u64::from(delay);
            db::jobqueue::retry(&app_state.pool, job_id, at).await?;
        }
    }
    Ok(())
}
