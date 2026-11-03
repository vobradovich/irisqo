use async_channel::Receiver;
use futures::{StreamExt, future::join_all};
use tokio::{select, task::JoinHandle, time};
use tracing::{error, info};

use crate::{
    db,
    models::{AppState, Error, JobWithRetry},
    services::jobrunner,
};
use std::{num::NonZeroUsize, sync::Arc, thread};

/// One long-lived worker task per core, each driving up to `per_worker` jobs
/// concurrently (`buffer_unordered`). Jobs are not spawned as tasks, and a slow job
/// never blocks the jobs of other workers: they all pull from the same channel.
#[derive(Debug)]
pub struct ConcurrentWorkerService {
    app_state: Arc<AppState>,
}

impl ConcurrentWorkerService {
    pub fn new(app_state: Arc<AppState>) -> Self {
        Self { app_state }
    }

    pub async fn run(&self) -> Result<(), Error> {
        let instance_id = &self.app_state.instance_id;
        let app_state: &Arc<AppState> = &self.app_state;
        // `--concurrency` is validated to be > 0 when this service is selected
        let concurrency = app_state.worker_options.concurrency.unwrap_or(1);

        let cores = thread::available_parallelism().map_or(1, NonZeroUsize::get);
        let workers_count = cores.min(concurrency);
        let per_worker = concurrency.div_ceil(workers_count);
        // Fetch in big enough batches to keep every worker busy: one db round trip per batch.
        let batch = usize::from(app_state.worker_options.prefetch).max(concurrency / 4);
        info!({ instance_id, workers_count, per_worker, batch }, "start");

        let mut interval = time::interval(app_state.worker_options.poll_interval);
        interval.set_missed_tick_behavior(time::MissedTickBehavior::Skip);

        let (tx, rx) = async_channel::bounded::<JobWithRetry>(batch);

        let running_workers: Vec<JoinHandle<()>> = (0..workers_count)
            .map(|idx| {
                let state = Arc::clone(app_state);
                let rx_worker = rx.clone();
                tokio::spawn(async move { run_worker(&state, idx, per_worker, rx_worker).await })
            })
            .collect();

        while !app_state.shutdown_token.is_cancelled() {
            let jobs = db::jobqueue::fetch_job_with_retry(
                &app_state.pool,
                instance_id,
                i32::try_from(batch).unwrap_or(i32::MAX),
            )
            .await
            .unwrap_or_else(|err| {
                error!(
                    { instance_id },
                    "db::jobqueue::fetch_job_with_retry {:?}", err
                );
                Vec::new()
            });
            let empty = jobs.is_empty();
            // The jobs are already assigned to this instance in the db,
            // so hand them all over (even on shutdown) instead of leaving them locked.
            for job in jobs {
                _ = tx.send(job).await;
            }
            if empty {
                select!(
                    biased;
                    _ = app_state.shutdown_token.cancelled() => {},
                    _ = interval.tick() => {},
                );
            }
        }
        tx.close();
        join_all(running_workers).await;
        info!({ instance_id }, "stop");
        Ok(())
    }
}

async fn run_worker(
    app_state: &Arc<AppState>,
    idx: usize,
    per_worker: usize,
    rx: Receiver<JobWithRetry>,
) {
    let instance_id = &app_state.instance_id;
    info!({ instance_id, idx, per_worker }, "run_worker");
    let state: &AppState = app_state;
    // Ends when the channel is closed and drained and all in-flight jobs are done.
    rx.map(|entry| jobrunner::job_run(state, entry))
        .buffer_unordered(per_worker)
        .for_each(|()| async {})
        .await;
    info!({ instance_id, idx }, "stop_worker");
}
