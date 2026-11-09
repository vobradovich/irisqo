use async_channel::Receiver;
use futures::StreamExt;
use tokio::{select, sync::mpsc, task::JoinSet, time};
use tracing::{error, info, warn};

use crate::{
    db,
    models::{AppState, JobWithRetry},
    services::jobrunner::{self, Completion},
};
use std::{sync::Arc, time::Duration};

/// Max results written by one statement.
const COMPLETE_BATCH: usize = 256;
/// Time for the last results after the workers stopped (`--shutdown-timeout` excluded).
const COMPLETER_GRACE: Duration = Duration::from_secs(5);

/// Loader -> workers -> completer.
///
/// - The loader assigns jobs to this instance in batches (one db round trip per batch).
/// - `--workers` long-lived worker tasks drive up to `--prefetch` jobs each concurrently
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
        let workers_count = app_state.worker_options.workers;
        let per_worker = usize::from(app_state.worker_options.prefetch).max(1);
        let concurrency = workers_count * per_worker;
        // one fetch fills every worker
        let batch = concurrency;
        info!({ instance_id, workers_count, per_worker, batch }, "start");

        // An empty queue is polled after `--interval`, then less often: 2x per empty poll up to
        // `--max-interval`. A claim that finds jobs starts over.
        let interval = app_state.worker_options.poll_interval;
        let max_interval = app_state.worker_options.max_poll_interval.max(interval);
        let mut pause = interval;

        let (tx, rx) = async_channel::bounded::<JobWithRetry>(batch);
        let (done_tx, done_rx) = mpsc::channel::<Completion>(concurrency);

        let completer = tokio::spawn({
            let state = Arc::clone(app_state);
            async move { run_completer(&state, done_rx).await }
        });
        let mut running_workers = JoinSet::new();
        for idx in 0..workers_count {
            let state = Arc::clone(app_state);
            let rx_worker = rx.clone();
            let done_tx = done_tx.clone();
            running_workers.spawn(async move {
                run_worker(&state, idx, per_worker, rx_worker, done_tx).await
            });
        }
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
            let wait = pause;
            pause = if empty {
                (pause * 2).min(max_interval)
            } else {
                interval
            };
            for job in jobs {
                // On shutdown the jobs not handed over yet stay assigned to this
                // instance: `kill` releases them on exit.
                select!(
                    biased;
                    () = app_state.shutdown_token.cancelled() => break,
                    // the channel is closed only below, after this loop
                    _ = tx.send(job) => {},
                );
            }
            if empty {
                select!(
                    biased;
                    () = app_state.shutdown_token.cancelled() => {},
                    () = time::sleep(wait) => {},
                );
            }
        }
        // Drain: workers finish their jobs, then the completer writes the last results.
        // Past `shutdown_timeout` the jobs still running are abandoned: they stay
        // assigned to this instance and `kill` releases them on exit.
        tx.close();
        let deadline = time::Instant::now() + app_state.worker_options.shutdown_timeout;
        let drained = time::timeout_at(deadline, async {
            while running_workers.join_next().await.is_some() {}
        });
        if drained.await.is_err() {
            warn!({ instance_id }, "shutdown timeout, running jobs abandoned");
            running_workers.abort_all();
        }
        // The aborted workers drop their senders: the completer writes what it got and ends.
        match time::timeout(COMPLETER_GRACE, completer).await {
            Ok(res) => {
                _ = res.inspect_err(|err| error!({ instance_id }, "completer error {:?}", err));
            }
            Err(_) => warn!({ instance_id }, "completer timeout, results not written"),
        }
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
