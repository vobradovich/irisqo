use futures::{TryStreamExt, future::join_all};
use tokio::{select, sync::RwLock, time};
use tracing::{debug, error};

use crate::{
    db,
    models::{AppState, Error},
    services::jobrunner,
};
use std::sync::Arc;

#[derive(Debug)]
pub struct TimerJobService {
    app_state: Arc<AppState>,
    running_workers: Arc<RwLock<Vec<tokio::task::JoinHandle<()>>>>,
}

impl TimerJobService {
    pub fn new(app_state: Arc<AppState>) -> Self {
        let running_workers = Arc::new(RwLock::new(Vec::new()));
        Self {
            app_state,
            running_workers,
        }
    }

    pub async fn run(&self) -> Result<(), Error> {
        debug!({ instance_id = self.app_state.instance_id }, "start");
        let mut interval = time::interval(self.app_state.worker_options.poll_interval);
        interval.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
        while !self.app_state.shutdown_token.is_cancelled() {
            if let Err(err) = self.tick().await {
                error!(
                    { instance_id = self.app_state.instance_id },
                    "error {}", err
                );
            }
            select!(
                _ = interval.tick() => {},
                _ = self.app_state.shutdown_token.cancelled() => {}
            );
        }
        join_all(self.running_workers.write().await.iter_mut()).await;
        debug!({ instance_id = self.app_state.instance_id }, "stop");
        Ok(())
    }

    async fn tick(&self) -> Result<(), Error> {
        self.trigger().await;
        Ok(())
    }

    async fn trigger(&self) {
        let worker_count = self.app_state.worker_options.workers_count;
        if worker_count == 0 {
            debug!({ instance_id = self.app_state.instance_id, worker_count }, "app_state.worker_options.workers_count equals to 0");
            return;
        }
        let len = self.running_workers.read().await.len();
        if len >= worker_count {
            // debug!({ instance_id = self.app_state.instance_id, worker_count }, "full");
            return;
        }
        let workers_poll_interval = self.app_state.worker_options.poll_interval;
        for idx in 0..(worker_count - len) {
            let join_handle = tokio::spawn({
                let state = Arc::clone(&self.app_state);
                async move { run_worker(state, idx).await }
            });
            self.running_workers.write().await.push(join_handle);

            tokio::time::sleep(workers_poll_interval / u32::try_from(worker_count).unwrap()).await;
        }
    }
}

pub async fn run_worker(app_state: Arc<AppState>, idx: usize) {
    debug!({ instance_id = app_state.instance_id, idx }, "run_worker");

    let mut interval = time::interval(app_state.worker_options.poll_interval);
    interval.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    while !app_state.shutdown_token.is_cancelled() {
        interval.tick().await;
        let state = Arc::clone(&app_state);
        if let Err(err) = run_job_batch(state).await {
            error!({ instance_id = app_state.instance_id }, "error {}", err);
        }
    }
    debug!({ instance_id = app_state.instance_id, idx }, "stop_worker");
}

async fn run_job_batch(app_state: Arc<AppState>) -> Result<(), Error> {
    // debug!({ instance_id = app_state.instance_id }, "run_job_batch");
    let mut rows = db::jobqueue::fetch_enqueued(
        &app_state.pool,
        &app_state.instance_id,
        app_state.worker_options.prefetch.into(),
    );

    while let Some(entry) = rows.try_next().await? {
        jobrunner::job_run(&app_state, entry).await;
    }
    Ok(())
}
