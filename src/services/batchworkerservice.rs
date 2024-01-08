use futures::{future::join_all, TryStreamExt};
use tokio::{select, time};
#[allow(unused_imports)]
use tracing::{debug, error, info, trace, warn};

use crate::{
    db,
    models::{AppState, Error},
    services::jobrunner,
};
use std::sync::Arc;

#[derive(Debug)]
pub struct BatchWorkerService {
    app_state: Arc<AppState>,
}

impl BatchWorkerService {
    pub fn new(app_state: Arc<AppState>) -> Self {
        Self { app_state }
    }

    pub async fn run(&self) -> Result<(), Error> {
        let instance_id = &self.app_state.instance_id;
        info!({ instance_id }, "start");
        let worker_count = self.app_state.worker_options.workers_count;
        if worker_count == 0 {
            debug!({ instance_id, worker_count }, "app_state.worker_options.workers_count equals to 0");
            return Ok(());
        }
        let workers_poll_interval = self.app_state.worker_options.poll_interval;
        let mut running_workers: Vec<tokio::task::JoinHandle<()>> = Vec::with_capacity(worker_count);
        for idx in 0..worker_count {
            let join_handle = tokio::spawn({
                let state = Arc::clone(&self.app_state);
                async move { run_worker(state, idx).await }
            });
            running_workers.push(join_handle);

            tokio::time::sleep(workers_poll_interval / u32::try_from(worker_count).unwrap()).await;
        }

        join_all(running_workers.iter_mut()).await;
        info!({ instance_id }, "stop");
        Ok(())
    }
}

pub async fn run_worker(app_state: Arc<AppState>, idx: usize) {
    let instance_id = &app_state.instance_id;
    info!({ instance_id, idx }, "run_worker");

    let mut interval = time::interval(app_state.worker_options.poll_interval);
    interval.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    while !app_state.shutdown_token.is_cancelled() {
        if let Err(err) = run_job_batch(&app_state).await {
            error!({ instance_id }, "error {}", err);
        }
        select!(
            biased;
            _ = app_state.shutdown_token.cancelled() => {}
            _ = interval.tick() => {},
        );
    }
    info!({ instance_id, idx }, "stop_worker");
}

async fn run_job_batch(app_state: &Arc<AppState>) -> Result<(), Error> {
    let instance_id = &app_state.instance_id;
    trace!({ instance_id }, "run_job_batch");
    let mut rows = db::jobqueue::fetch_enqueued(
        &app_state.pool,
        instance_id,
        i32::from(app_state.worker_options.prefetch),
    );

    while let Some(entry) = rows.try_next().await? {
        jobrunner::job_run(app_state, entry).await;
    }
    Ok(())
}
