#[cfg(feature = "batch-worker")]
mod batchworkerservice;
mod channelworkerservice;
pub mod jobrunner;
#[cfg(feature = "naive-worker")]
mod naiveworkerservice;
mod schedulerservice;
#[cfg(feature = "timer-worker")]
mod timerjobservice;

use crate::models::AppState;
use std::sync::Arc;

pub async fn start_scheduler_service(state: &Arc<AppState>) {
    let app_state = Arc::clone(state);
    let service = schedulerservice::SchedulerService::new(app_state);
    service.run().await.expect("Failed to run SchedulerService");
}

pub async fn start_channel_worker_service(state: &Arc<AppState>) {
    let app_state = Arc::clone(state);
    let service = channelworkerservice::ChannelWorkerService::new(app_state);
    service
        .run()
        .await
        .expect("Failed to run ChannelWorkerService");
}

#[cfg(feature = "batch-worker")]
pub async fn start_batch_jobs_service(state: &Arc<AppState>) {
    let app_state = Arc::clone(state);
    let service = batchworkerservice::BatchWorkerService::new(app_state);
    service.run().await.expect("Failed to run JobService");
}

#[cfg(feature = "naive-worker")]
pub async fn start_naive_jobs_service(state: &Arc<AppState>) {
    let app_state = Arc::clone(state);
    let service = NaiveWorkerService::new(app_state);
    service.run().await.expect("Failed to run JobService");
}

#[cfg(feature = "timer-worker")]
pub async fn start_timer_jobs_service(state: &Arc<AppState>) {
    let app_state = Arc::clone(state);
    let service = TimerJobService::new(app_state);
    service.run().await.expect("Failed to run JobService");
}
