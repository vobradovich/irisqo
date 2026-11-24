use async_channel::Receiver;
use futures::StreamExt;
use tokio::{select, sync::mpsc, task::JoinSet, time};
use tracing::{error, info, warn};

use crate::{
    db,
    models::{AppState, JobWithAttempt},
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
///   (`for_each_concurrent`); they all pull from the same channel, so a slow job never
///   blocks the others.
/// - The completer writes the final results: whatever has piled up while the previous
///   write was running goes into the next statement, so under load results are written
///   in batches, and a lone result is written right away.
/// - A written result or retry that makes a job claimable (a freed place of a flow, a callback)
///   wakes the loader from its pause: the freed place is taken at once, not at the next poll.
///   So do the other parts of this process that queue a job ([`AppState::wake_workers`]):
///   the API, the UI and the scheduler, unless `--no-wake`.
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
        let options = &app_state.options.worker;
        let workers_count = options.workers;
        let per_worker = usize::from(options.prefetch).max(1);
        let concurrency = workers_count * per_worker;
        // one fetch fills every worker
        let batch = concurrency;
        info!({ instance_id, workers_count, per_worker, batch }, "start");

        // an empty queue backs off 2x per empty poll up to `--max-interval`; jobs reset it
        let interval = options.poll_interval;
        let max_interval = options.max_poll_interval.max(interval);
        let mut pause = interval;

        let (tx, rx) = async_channel::bounded::<JobWithAttempt>(batch);
        let (done_tx, done_rx) = mpsc::channel::<Completion>(concurrency);
        // a permit stored before this loop (a job created at the start) only costs one fetch
        let wake = &app_state.wake;

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
            let jobs = db::jobqueue::fetch_job_with_attempt(
                &app_state.pool,
                instance_id,
                i32::try_from(batch).unwrap_or(i32::MAX),
            )
            .await
            .inspect_err(|err| {
                error!(
                    { instance_id },
                    "db::jobqueue::fetch_job_with_attempt {:?}", err
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
                // on shutdown the jobs not handed over stay assigned; `kill` releases them on exit
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
                    () = wake.notified() => pause = interval,
                    () = time::sleep(wait) => {},
                );
            }
        }
        // drain: workers finish, then the completer; past `shutdown_timeout` `kill` frees the rest
        tx.close();
        let deadline = time::Instant::now() + options.shutdown_timeout;
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
    rx: Receiver<JobWithAttempt>,
    done_tx: mpsc::Sender<Completion>,
) {
    let instance_id = &app_state.instance_id;
    info!({ instance_id, idx, per_worker }, "run_worker");
    let state: &AppState = app_state;
    let done_tx = &done_tx;
    // Ends when the channel is closed and drained and all in-flight jobs are done.
    // Each job sends its own result: a job waiting for room in the completer's channel must
    // not stop the others. (`buffer_unordered` + an await in `for_each` stops polling them:
    // a stopped job that holds a pool connection keeps it, the completer cannot get one to
    // make room, and all of them wait until the completer's write gives up.)
    rx.for_each_concurrent(per_worker, |entry| async move {
        let in_flow = entry.job.meta.flow_key.is_some();
        match jobrunner::job_execute(state, entry).await {
            // the completer outlives the workers: send fails only if it panicked
            Some(completion) => _ = done_tx.send(completion).await,
            // a retry is written already: it freed the place of the job in its flow
            None if in_flow => state.wake.notify_one(),
            None => {}
        }
    })
    .await;
    info!({ instance_id, idx }, "stop_worker");
}

/// Ends when every worker is gone and all results are written.
async fn run_completer(app_state: &AppState, mut rx: mpsc::Receiver<Completion>) {
    let mut buf = Vec::with_capacity(COMPLETE_BATCH);
    while rx.recv_many(&mut buf, COMPLETE_BATCH).await > 0 {
        let wakes = buf.iter().any(|c| c.wakes);
        jobrunner::complete(app_state, std::mem::take(&mut buf)).await;
        if wakes {
            app_state.wake.notify_one();
        }
    }
}
