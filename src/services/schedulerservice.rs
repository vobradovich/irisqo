use tokio::{select, time};
#[allow(unused_imports)]
use tracing::{debug, error, info, trace, warn};

use crate::{
    db, features,
    models::{AppState, Error},
};
use std::{sync::Arc, time::Duration};

#[derive(Debug)]
pub struct SchedulerService {
    app_state: Arc<AppState>,
}

impl SchedulerService {
    pub fn new(app_state: Arc<AppState>) -> Self {
        Self { app_state }
    }

    pub async fn run(&self) -> Result<(), Error> {
        let instance_id = &self.app_state.instance_id;
        if self.app_state.scheduler_options.is_none() {
            warn!({ instance_id }, "disabled");
            return Ok(());
        }
        info!({ instance_id }, "start");
        let t = self
            .app_state
            .scheduler_options
            .as_ref()
            .unwrap()
            .poll_interval;
        let mut interval = time::interval(t);
        interval.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
        while !self.app_state.shutdown_token.is_cancelled() {
            if let Err(err) = self.tick().await {
                error!({ instance_id }, "error {}", err);
            }
            select!(
                biased;
                _ = self.app_state.shutdown_token.cancelled() => {}
                _ = interval.tick() => {},
            );
        }
        db::instances::kill(&self.app_state.pool, instance_id).await?;
        info!({ instance_id }, "stop");
        Ok(())
    }

    async fn tick(&self) -> Result<(), Error> {
        let instance_id = &self.app_state.instance_id;
        let pool = &self.app_state.pool;
        trace!({ instance_id }, "tick");
        features::instances::live(pool, instance_id).await?;
        let expired = db::instances::kill_expired(pool, Duration::from_secs(30)).await?;
        let enqueued = db::jobqueue::enqueue_scheduled(pool, instance_id).await?;
        debug!({ instance_id, enqueued, expired }, "db::jobs::enqueue_scheduled");
        Ok(())
    }
}
