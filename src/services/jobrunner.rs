use crate::{
    db,
    models::{AppState, Error, JobEntry, JobProtocol, JobRow},
};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::time;
#[allow(unused_imports)]
use tracing::{debug, error, info, warn};

pub async fn job_run(app_state: &AppState, entry: JobEntry) {
    let run_result = job_run_with_error(&app_state, entry).await;
    if let Err(err) = run_result {
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
        JobProtocol::Null => Ok(()),
        JobProtocol::Http(_) => job_run_http(app_state, job).await,
    };
    match job_result {
        Ok(_) => {
            db::jobqueue::succeed(&app_state.pool, job_id).await?;
        }
        Err(err) => match err {
            Error::HyperError(_) | Error::Timeout(_) => {
                let retry: u16 = entry.retry.try_into().unwrap_or(0);
                let now_secs = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs();
                match meta.retry.next_retry_in(retry) {
                    None => {
                        warn!({ instance_id = app_state.instance_id, job_id }, "====> fail {:?}", err);
                        db::jobqueue::fail(&app_state.pool, job_id).await?;
                    }
                    Some(0) => {
                        info!({ instance_id = app_state.instance_id, job_id }, "====> unlock {:?}", err);
                        db::jobqueue::unlock(&app_state.pool, job_id).await?;
                    }
                    Some(delay) => {
                        let at = now_secs + u64::from(delay);
                        info!({ instance_id = app_state.instance_id, job_id }, "====> retry in {} {:?}", delay, err);
                        db::jobqueue::retry(&app_state.pool, job_id, at).await?;
                    }
                }
            }
            _ => {
                error!({ instance_id = app_state.instance_id, job_id }, "====> error {:?}", err);
                db::jobqueue::fail(&app_state.pool, job_id).await?;
            }
        },
    }
    Ok(())
}

async fn job_run_http(app_state: &AppState, job: JobRow) -> Result<(), Error> {
    let job_id = job.id;
    let timeout_ms = job.meta.timeout;
    let req = hyper::Request::<hyper::Body>::try_from(job)?;
    let future = app_state.client.request(req);
    // first '?' - timeout
    // second '?' - HyperError
    let response = time::timeout(Duration::from_millis(timeout_ms.into()), future).await??;
    debug!({ instance_id = app_state.instance_id, job_id }, "====> response={:?}", response);
    Ok(())
}
