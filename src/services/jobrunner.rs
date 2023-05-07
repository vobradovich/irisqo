use std::time::{SystemTime, UNIX_EPOCH};

use crate::{
    db,
    models::{AppState, Error, JobProtocol, JobQueueRow, JobRetry, JobRow},
};
#[allow(unused_imports)]
use tracing::{debug, error, info, warn};

pub async fn job_run(app_state: &AppState, entry: JobQueueRow) -> Result<(), Error> {
    let job_id = entry.id;
    let job = db::jobqueue::get_by_id(&app_state.pool, job_id)
        .await?
        .ok_or(Error::JobNotFound(job_id))?;
    let meta = job.meta.0.clone();
    let res = match meta.protocol {
        JobProtocol::Null => Ok(()),
        JobProtocol::Http(_) => job_run_http(app_state, job).await,
    };
    match res {
        Ok(_) => {
            db::jobqueue::succeed(&app_state.pool, job_id).await?;
        }
        Err(err) => match err {
            Error::HyperError(_) => {
                warn!({ instance_id = app_state.instance_id, job_id }, "call error {:?}", err);
                let retry = u32::try_from(entry.retry).unwrap_or(0);
                let now_secs = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs();
                match meta.retry {
                    JobRetry::None => {
                        db::jobqueue::fail(&app_state.pool, job_id).await?;
                    }
                    JobRetry::Immediate { retry_count } => {
                        if retry >= retry_count {
                            db::jobqueue::fail(&app_state.pool, job_id).await?;
                        } else {
                            db::jobqueue::retry(&app_state.pool, job_id, None).await?;
                        }
                    }
                    JobRetry::Fixed {
                        retry_count,
                        retry_delay,
                    } => {
                        if retry >= retry_count {
                            db::jobqueue::fail(&app_state.pool, job_id).await?;
                        } else {
                            db::jobqueue::retry(
                                &app_state.pool,
                                job_id,
                                Some(now_secs + u64::from(retry_delay)),
                            )
                            .await?;
                        }
                    }
                    JobRetry::Fibonacci {
                        retry_count,
                        retry_delay,
                    } => {
                        if retry >= retry_count {
                            db::jobqueue::fail(&app_state.pool, job_id).await?;
                        } else {
                            let delay = u64::from(retry_delay * JobRetry::fibonacci(retry as usize));
                            warn!({ instance_id = app_state.instance_id, job_id }, "delay {:?}", delay);
                            db::jobqueue::retry(
                                &app_state.pool,
                                job_id,
                                Some(now_secs + delay),
                            )
                            .await?;
                        }
                    }
                }
            }
            _ => {
                error!({ instance_id = app_state.instance_id, job_id }, "error {:?}", err);
                db::jobqueue::fail(&app_state.pool, job_id).await?;
            }
        },
    }
    Ok(())
}

async fn job_run_http(app_state: &AppState, job: JobRow) -> Result<(), Error> {
    let job_id = job.id;
    let req = hyper::Request::<hyper::Body>::try_from(job)?;
    let res = app_state.client.request(req).await?;
    debug!({ instance_id = app_state.instance_id, job_id }, "response={:?}", res);
    Ok(())
}
