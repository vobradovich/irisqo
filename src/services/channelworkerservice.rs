use async_channel::Receiver;
use futures::{future::join_all, StreamExt};
use tokio::{select, task::JoinHandle, time};
#[allow(unused_imports)]
use tracing::{debug, error, info, warn};

use crate::{
    db,
    models::{AppState, Error, JobWithRetry},
    services::jobrunner,
};
use std::sync::Arc;

#[derive(Debug)]
pub struct ChannelWorkerService {
    app_state: Arc<AppState>,
}

impl ChannelWorkerService {
    pub fn new(app_state: Arc<AppState>) -> Self {
        Self { app_state }
    }

    pub async fn run(&self) -> Result<(), Error> {
        let instance_id = &self.app_state.instance_id;
        let app_state: &Arc<AppState> = &self.app_state;
        info!({ instance_id }, "start");

        let workers_count = app_state.worker_options.workers_count;
        if workers_count == 0 {
            debug!({ instance_id, workers_count }, "app_state.worker_options.workers_count equals to 0");
            return Ok(());
        }

        let mut interval = time::interval(app_state.worker_options.poll_interval);
        interval.set_missed_tick_behavior(time::MissedTickBehavior::Skip);

        let (tx, rx) = async_channel::bounded::<JobWithRetry>(workers_count);

        let mut running_workers: Vec<JoinHandle<()>> = Vec::with_capacity(workers_count);
        for idx in 0..workers_count {
            let join_handle = tokio::spawn({
                let state = Arc::clone(app_state);
                let rx_worker = rx.clone();
                async move { run_worker(&state, idx, rx_worker).await }
            });
            running_workers.push(join_handle);
        }

        let mut empty: bool;
        while !app_state.shutdown_token.is_cancelled() {
            let mut rows = db::jobqueue::fetch_job_with_retry(
                &app_state.pool,
                instance_id,
                i32::from(app_state.worker_options.prefetch),
            );
            empty = true;
            loop {
                select!(
                    biased;
                    _ = app_state.shutdown_token.cancelled() => break,
                    res = rows.next() => match res {
                        Some(Ok(entry)) => {
                            _ = tx.send(entry).await;
                            empty = false;
                        },
                        Some(Err(err)) => {
                            error!({ instance_id }, "db::jobqueue::fetch_job_with_retry {:?}", err);
                            break;
                        },
                        _ => break,
                    }
                );
            }
            if empty {
                wait_tick_or_shutdown(&mut interval, app_state).await;
            }
        }
        tx.close();
        join_all(running_workers.iter_mut()).await;
        info!({ instance_id }, "stop");
        Ok(())
    }
}

async fn wait_tick_or_shutdown(interval: &mut time::Interval, app_state: &Arc<AppState>) {
    select!(
        biased;
        _ = app_state.shutdown_token.cancelled() => {},
        _ = interval.tick() => {},
    );
}

pub async fn run_worker(app_state: &Arc<AppState>, idx: usize, rx: Receiver<JobWithRetry>) {
    let instance_id = &app_state.instance_id;
    info!({ instance_id, idx }, "run_worker");
    while let Ok(entry) = rx.recv().await {
        jobrunner::job_run(app_state, entry).await;
    }
    info!({ instance_id, idx }, "stop_worker");
}
