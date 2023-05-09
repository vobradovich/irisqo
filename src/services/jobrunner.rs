use crate::{
    db,
    models::{AppState, Error, JobProtocol, JobQueueRow, JobRow},
};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::time;
#[allow(unused_imports)]
use tracing::{debug, error, info, warn};

pub async fn job_run(app_state: &AppState, entry: JobQueueRow) -> Result<(), Error> {
    let job_id = entry.id;
    let retry = entry.retry;
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
                warn!({ instance_id = app_state.instance_id, job_id }, "====> call error {:?}", err);
                let retry: u16 = entry.retry.try_into().unwrap_or(0);
                let now_secs = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs();
                match meta.retry.next_retry_in(retry) {
                    None => {
                        db::jobqueue::fail(&app_state.pool, job_id).await?;
                    }
                    Some(0) => {
                        db::jobqueue::retry(&app_state.pool, job_id, None).await?;
                    }
                    Some(delay) => {
                        db::jobqueue::retry(
                            &app_state.pool,
                            job_id,
                            Some(now_secs + u64::from(delay)),
                        )
                        .await?;
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
