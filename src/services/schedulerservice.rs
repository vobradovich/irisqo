use tokio::{select, time};
#[allow(unused_imports)]
use tracing::{debug, error, info, trace, warn};

use crate::{
    db,
    models::AppState,
    otel::metrics::metrics,
    services::{drain_batches, heartbeat::INSTANCE_EXPIRE},
};
use std::sync::Arc;

/// Lock reaper batch: stuck locks are rare, a few per tick is enough.
const REAP_LIMIT: i32 = 1000;

#[derive(Debug)]
pub struct SchedulerService {
    app_state: Arc<AppState>,
}

impl SchedulerService {
    pub fn new(app_state: Arc<AppState>) -> Self {
        Self { app_state }
    }

    pub async fn run(&self) {
        let instance_id = &self.app_state.instance_id;
        info!({ instance_id }, "start");
        let mut interval = time::interval(self.app_state.options.scheduler.poll_interval);
        interval.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
        while !self.app_state.shutdown_token.is_cancelled() {
            self.tick().await;
            select!(
                biased;
                () = self.app_state.shutdown_token.cancelled() => {}
                _ = interval.tick() => {},
            );
        }
        info!({ instance_id }, "stop");
    }

    /// One pass; every step runs (and logs its error) on its own.
    pub async fn tick(&self) {
        let instance_id = &self.app_state.instance_id;
        let pool = &self.app_state.pool;
        let lock_lease = self.app_state.options.lock_lease();
        trace!({ instance_id }, "tick");
        _ = db::instances::kill_expired(pool, INSTANCE_EXPIRE, instance_id)
            .await
            .inspect(|&expired| {
                if expired > 0 {
                    metrics().instances_expired.add(expired, &[]);
                    warn!({ instance_id, expired }, "expired instances killed");
                    // their jobs are back in the queue
                    self.app_state.wake_workers();
                }
            })
            .inspect_err(|err| error!({ instance_id }, "kill_expired error {:?}", err));
        _ = db::jobqueue::reap_expired_locks(pool, lock_lease, instance_id, REAP_LIMIT)
            .await
            .inspect(|&reaped| {
                if reaped > 0 {
                    metrics().locks_expired.add(reaped, &[]);
                    warn!({ instance_id, reaped }, "expired locks released");
                    self.app_state.wake_workers();
                }
            })
            .inspect_err(|err| error!({ instance_id }, "reap_expired_locks error {:?}", err));
        self.enqueue_due().await;
    }

    /// Moves every due job to the queue, `prefetch` at a time.
    async fn enqueue_due(&self) {
        let state = &self.app_state;
        let limit = i32::from(state.options.scheduler.prefetch.max(1));
        let total = drain_batches(
            state,
            "enqueue_scheduled",
            limit,
            usize::MAX,
            || async move {
                // each batch is committed on its own: its jobs are claimable before the next one
                db::jobqueue::enqueue_scheduled(&state.pool, &state.instance_id, limit)
                    .await
                    .inspect(|&queued| {
                        if queued > 0 {
                            state.wake_workers();
                        }
                    })
            },
        )
        .await;
        if total > 0 {
            debug!({ instance_id = state.instance_id, total }, "enqueued scheduled jobs");
        }
    }
}
