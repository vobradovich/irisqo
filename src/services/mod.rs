pub mod heartbeat;
pub mod jobrunner;
pub mod retentionservice;
pub mod schedulerservice;
pub mod workerservice;

use crate::models::AppState;
use std::sync::Arc;

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
