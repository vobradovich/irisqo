pub mod heartbeat;
pub mod jobrunner;
pub mod queuestats;
pub mod retentionservice;
pub mod schedulerservice;
pub mod workerservice;

use crate::models::{AppState, Error};
use std::sync::Arc;
use tracing::error;

/// Repeats a batch statement while it fills whole batches of `limit` rows, at most
/// `max_batches` times; stops on shutdown or on the first error (logged as `what`).
/// Returns the total of affected rows.
async fn drain_batches<F, Fut>(
    app_state: &AppState,
    what: &str,
    limit: i32,
    max_batches: usize,
    mut batch: F,
) -> u64
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<u64, Error>>,
{
    let instance_id = &app_state.instance_id;
    let mut total = 0;
    for _ in 0..max_batches {
        if app_state.shutdown_token.is_cancelled() {
            break;
        }
        let Ok(affected) = batch()
            .await
            .inspect_err(|err| error!({ instance_id }, "{what} error {:?}", err))
        else {
            break;
        };
        total += affected;
        if affected < u64::from(limit.unsigned_abs()) {
            break;
        }
    }
    total
}

pub async fn start_scheduler_service(state: &Arc<AppState>) {
    schedulerservice::SchedulerService::new(Arc::clone(state))
        .run()
        .await;
}

pub async fn start_retention_service(state: &Arc<AppState>) {
    retentionservice::RetentionService::new(Arc::clone(state))
        .run()
        .await;
}

pub async fn start_worker_service(state: &Arc<AppState>) {
    workerservice::WorkerService::new(Arc::clone(state))
        .run()
        .await;
}
