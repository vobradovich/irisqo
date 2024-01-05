use async_channel::Receiver;
use futures::future::join_all;
use tokio::{select, sync::RwLock, time};
#[allow(unused_imports)]
use tracing::{debug, error, info, warn};

use crate::{
    db,
    models::{AppState, Error, JobEntry},
    services::jobrunner,
};
use std::sync::Arc;

#[derive(Debug)]
pub struct ChannelWorkerService {
    app_state: Arc<AppState>,
    running_workers: Arc<RwLock<Vec<tokio::task::JoinHandle<()>>>>,
}

impl ChannelWorkerService {
    pub fn new(app_state: Arc<AppState>) -> Self {
        let running_workers = Arc::new(RwLock::new(Vec::new()));
        Self {
            app_state,
            running_workers,
        }
    }

    pub async fn run(&self) -> Result<(), Error> {
        let app_state: &Arc<AppState> = &self.app_state;
        info!({ instance_id = app_state.instance_id }, "start");

        let worker_count = app_state.worker_options.workers_count;
        if worker_count == 0 {
            debug!({ instance_id = app_state.instance_id, worker_count }, "app_state.worker_options.workers_count equals to 0");
            return Ok(());
        }

        let mut interval = time::interval(app_state.worker_options.poll_interval);
        interval.set_missed_tick_behavior(time::MissedTickBehavior::Skip);

        let (tx, rx) = async_channel::bounded::<JobEntry>(1);

        for idx in 0..worker_count {
            let join_handle = tokio::spawn({
                let state = Arc::clone(app_state);
                let rx_worker = rx.clone();
                async move { run_worker(state, idx, rx_worker).await }
            });
            self.running_workers.write().await.push(join_handle);
        }

        while !app_state.shutdown_token.is_cancelled() {
            let fetch_result =
                db::jobqueue::fetch_optional(&app_state.pool, &app_state.instance_id).await;
            match fetch_result {
                Ok(Some(entry)) => {
                    let _ = tx.send(entry).await;
                }
                Ok(None) => {
                    wait_tick_or_shutdown(&mut interval, app_state).await;
                }
                Err(err) => {
                    error!({ instance_id = app_state.instance_id }, "error {}", err);
                    wait_tick_or_shutdown(&mut interval, app_state).await;
                }
            }
        }
        tx.close();
        join_all(self.running_workers.write().await.iter_mut()).await;
        info!({ instance_id = app_state.instance_id }, "stop");
        Ok(())
    }
}

async fn wait_tick_or_shutdown(interval: &mut time::Interval, app_state: &Arc<AppState>) {
    //_ = time::timeout(app_state.worker_options.poll_interval, app_state.shutdown_token.cancelled()).await;
    select!(
        biased;
        _ = app_state.shutdown_token.cancelled() => {}
        _ = interval.tick() => {},
    );
}

pub async fn run_worker(app_state: Arc<AppState>, idx: usize, rx: Receiver<JobEntry>) {
    info!({ instance_id = app_state.instance_id, idx }, "run_worker");
    while let Ok(entry) = rx.recv().await {
        jobrunner::job_run(&app_state, entry).await;
    }
    info!({ instance_id = app_state.instance_id, idx }, "stop_worker");
}
