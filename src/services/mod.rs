mod channelworkerservice;
mod concurrentworkerservice;
pub mod jobrunner;
mod schedulerservice;

use crate::models::AppState;
use std::sync::Arc;

pub async fn start_scheduler_service(state: &Arc<AppState>) {
    let app_state = Arc::clone(state);
    let service = schedulerservice::SchedulerService::new(app_state);
    service.run().await.expect("Failed to run SchedulerService");
}

/// Runs `ConcurrentWorkerService` when `--concurrency` is set, `ChannelWorkerService` otherwise.
pub async fn start_worker_service(state: &Arc<AppState>) {
    if state.worker_options.concurrency.is_some() {
        start_concurrent_worker_service(state).await;
    } else {
        start_channel_worker_service(state).await;
    }
}

async fn start_channel_worker_service(state: &Arc<AppState>) {
    let app_state = Arc::clone(state);
    let service = channelworkerservice::ChannelWorkerService::new(app_state);
    service
        .run()
        .await
        .expect("Failed to run ChannelWorkerService");
}

async fn start_concurrent_worker_service(state: &Arc<AppState>) {
    let app_state = Arc::clone(state);
    let service = concurrentworkerservice::ConcurrentWorkerService::new(app_state);
    service
        .run()
        .await
        .expect("Failed to run ConcurrentWorkerService");
}
