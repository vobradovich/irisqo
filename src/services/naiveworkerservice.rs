use futures::future::join_all;
use tokio::{select, sync::RwLock, time};
#[allow(unused_imports)]
use tracing::{debug, error, info, warn};

use crate::{
    db,
    models::{AppState, Error},
    services::jobrunner,
};
use std::sync::Arc;

#[derive(Debug)]
pub struct NaiveWorkerService {
    app_state: Arc<AppState>,
    running_workers: Arc<RwLock<Vec<tokio::task::JoinHandle<()>>>>,
}

impl NaiveWorkerService {
    pub fn new(app_state: Arc<AppState>) -> Self {
        let running_workers = Arc::new(RwLock::new(Vec::new()));
        Self {
            app_state,
            running_workers,
        }
    }

    pub async fn run(&self) -> Result<(), Error> {
        info!({ instance_id = self.app_state.instance_id }, "start");

        let worker_count = self.app_state.worker_options.workers_count.unwrap_or(0);
        if worker_count == 0 {
            debug!({ instance_id = self.app_state.instance_id, worker_count }, "app_state.worker_options.workers_count equals to 0");
            return Ok(());
        }
        let workers_poll_interval = self.app_state.worker_options.poll_interval;
        for idx in 0..worker_count {
            let join_handle = tokio::spawn({
                let state = Arc::clone(&self.app_state);
                async move { run_worker(state, idx).await }
            });
            self.running_workers.write().await.push(join_handle);

            tokio::time::sleep(workers_poll_interval / u32::try_from(worker_count).unwrap()).await;
        }

        join_all(self.running_workers.write().await.iter_mut()).await;
        info!({ instance_id = self.app_state.instance_id }, "stop");
        Ok(())
    }
}

pub async fn run_worker(app_state: Arc<AppState>, idx: usize) {
    info!({ instance_id = app_state.instance_id, idx }, "run_worker");

    let mut interval = time::interval(app_state.worker_options.poll_interval);
    interval.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    while !app_state.shutdown_token.is_cancelled() {
        match run_job(&app_state).await {
            Ok(true) => (),
            Ok(false) => {
                select!(
                    _ = interval.tick() => {},
                    _ = app_state.shutdown_token.cancelled() => {}
                );
            }
            Err(err) => {
                error!({ instance_id = app_state.instance_id }, "error {}", err);
                select!(
                    _ = interval.tick() => {},
                    _ = app_state.shutdown_token.cancelled() => {}
                );
            }
        }
    }
    info!({ instance_id = app_state.instance_id, idx }, "stop_worker");
}

async fn run_job(app_state: &AppState) -> Result<bool, Error> {
    let row = db::jobqueue::fetch_optional(&app_state.pool, &app_state.instance_id).await?;
    if let Some(entry) = row {
        jobrunner::job_run(app_state, entry).await;
        return Ok(true);
    }
    Ok(false)
}
