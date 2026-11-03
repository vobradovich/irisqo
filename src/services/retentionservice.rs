use std::{sync::Arc, time::Duration};

use tokio::{select, time};
use tracing::{debug, error, info};

use crate::{db, models::AppState};

const RETENTION_INTERVAL: Duration = Duration::from_secs(60);
const PURGE_BATCH: i32 = 1000;
/// Per run, so that a big backlog is purged gradually.
const MAX_BATCHES: u32 = 100;

#[derive(Debug)]
pub struct RetentionService {
    app_state: Arc<AppState>,
}

impl RetentionService {
    pub fn new(app_state: Arc<AppState>) -> Self {
        Self { app_state }
    }

    pub async fn run(&self) {
        let instance_id = &self.app_state.instance_id;
        let hours = self
            .app_state
            .scheduler_options
            .as_ref()
            .map_or(0, |o| o.retention_hours);
        if hours == 0 {
            info!({ instance_id }, "disabled");
            return;
        }
        let retention = Duration::from_secs(u64::from(hours) * 3600);
        info!({ instance_id, retention_secs = retention.as_secs() }, "start");
        let mut interval = time::interval(RETENTION_INTERVAL);
        interval.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
        loop {
            select!(
                biased;
                () = self.app_state.shutdown_token.cancelled() => break,
                _ = interval.tick() => {},
            );
            self.purge(retention).await;
        }
        info!({ instance_id }, "stop");
    }

    /// Deletes everything older than `retention`.
    pub async fn purge(&self, retention: Duration) {
        let instance_id = &self.app_state.instance_id;
        let pool = &self.app_state.pool;
        let mut jobs = 0;
        for _ in 0..MAX_BATCHES {
            if self.app_state.shutdown_token.is_cancelled() {
                break;
            }
            let Ok(deleted) = db::retention::purge_processed(pool, retention, PURGE_BATCH)
                .await
                .inspect_err(|err| error!({ instance_id }, "purge_processed error {:?}", err))
            else {
                break;
            };
            jobs += deleted;
            if deleted < u64::from(PURGE_BATCH.unsigned_abs()) {
                break;
            }
        }
        let schedules = db::retention::purge_schedules(pool)
            .await
            .inspect_err(|err| error!({ instance_id }, "purge_schedules error {:?}", err))
            .unwrap_or_default();
        let instances = db::retention::purge_instances(pool, retention)
            .await
            .inspect_err(|err| error!({ instance_id }, "purge_instances error {:?}", err))
            .unwrap_or_default();
        if jobs + schedules + instances > 0 {
            debug!({ instance_id, jobs, schedules, instances }, "purged");
        }
    }
}
