use async_channel::Receiver;
use futures::{StreamExt, future::join_all};
use tokio::{select, sync::mpsc, task::JoinHandle, time};
use tracing::{error, info};

use crate::{
    db,
    models::{AppState, JobWithRetry},
    services::jobrunner::{self, Completion},
};
use std::{num::NonZeroUsize, sync::Arc, thread};

/// Max results written by one statement.
const COMPLETE_BATCH: usize = 256;

/// Loader -> workers -> completer.
///
/// - The loader assigns jobs to this instance in batches (one db round trip per batch).
/// - One long-lived worker task per core drives up to `per_worker` jobs concurrently
///   (`buffer_unordered`); they all pull from the same channel, so a slow job never
///   blocks the others.
/// - The completer writes the final results: whatever has piled up while the previous
///   write was running goes into the next statement, so under load results are written
///   in batches, and a lone result is written right away.
#[derive(Debug)]
pub struct WorkerService {
    app_state: Arc<AppState>,
}

impl WorkerService {
    pub fn new(app_state: Arc<AppState>) -> Self {
        Self { app_state }
    }

    pub async fn run(&self) {
        let instance_id = &self.app_state.instance_id;
        let app_state: &Arc<AppState> = &self.app_state;
        let concurrency = app_state.worker_options.concurrency.max(1);

        let cores = thread::available_parallelism().map_or(1, NonZeroUsize::get);
        let workers_count = cores.min(concurrency);
        let per_worker = concurrency.div_ceil(workers_count);
        let batch = usize::from(app_state.worker_options.prefetch).max(concurrency / 4);
        info!({ instance_id, workers_count, per_worker, batch }, "start");

        let mut interval = time::interval(app_state.worker_options.poll_interval);
        interval.set_missed_tick_behavior(time::MissedTickBehavior::Skip);

        let (tx, rx) = async_channel::bounded::<JobWithRetry>(batch);
        let (done_tx, done_rx) = mpsc::channel::<Completion>(concurrency);

        let completer = tokio::spawn({
            let state = Arc::clone(app_state);
            async move { run_completer(&state, done_rx).await }
        });
        let running_workers: Vec<JoinHandle<()>> = (0..workers_count)
            .map(|idx| {
                let state = Arc::clone(app_state);
                let rx_worker = rx.clone();
                let done_tx = done_tx.clone();
                tokio::spawn(async move {
                    run_worker(&state, idx, per_worker, rx_worker, done_tx).await;
                })
            })
            .collect();
        drop(done_tx);

        while !app_state.shutdown_token.is_cancelled() {
            let jobs = db::jobqueue::fetch_job_with_retry(
                &app_state.pool,
                instance_id,
                i32::try_from(batch).unwrap_or(i32::MAX),
            )
            .await
            .inspect_err(|err| {
                error!(
                    { instance_id },
                    "db::jobqueue::fetch_job_with_retry {:?}", err
                );
            })
            .unwrap_or_default();
            let empty = jobs.is_empty();
            // The jobs are already assigned to this instance in the db,
            // so hand them all over (even on shutdown) instead of leaving them locked.
            for job in jobs {
                _ = tx.send(job).await;
            }
            if empty {
                select!(
                    biased;
                    () = app_state.shutdown_token.cancelled() => {},
                    _ = interval.tick() => {},
                );
            }
        }
        // Drain: workers finish their jobs, then the completer writes the last results.
        tx.close();
        join_all(running_workers).await;
        _ = completer
            .await
            .inspect_err(|err| error!({ instance_id }, "completer error {:?}", err));
        info!({ instance_id }, "stop");
    }
}

async fn run_worker(
    app_state: &Arc<AppState>,
    idx: usize,
    per_worker: usize,
    rx: Receiver<JobWithRetry>,
    done_tx: mpsc::Sender<Completion>,
) {
    let instance_id = &app_state.instance_id;
    info!({ instance_id, idx, per_worker }, "run_worker");
    let state: &AppState = app_state;
    // Ends when the channel is closed and drained and all in-flight jobs are done.
    rx.map(|entry| jobrunner::job_execute(state, entry))
        .buffer_unordered(per_worker)
        .for_each(|completion| async {
            if let Some(completion) = completion {
                // the completer outlives the workers: send fails only if it panicked
                _ = done_tx.send(completion).await;
            }
        })
        .await;
    info!({ instance_id, idx }, "stop_worker");
}

/// Ends when every worker is gone and all results are written.
async fn run_completer(app_state: &AppState, mut rx: mpsc::Receiver<Completion>) {
    let mut buf = Vec::with_capacity(COMPLETE_BATCH);
    while rx.recv_many(&mut buf, COMPLETE_BATCH).await > 0 {
        jobrunner::complete(app_state, std::mem::take(&mut buf)).await;
    }
}
