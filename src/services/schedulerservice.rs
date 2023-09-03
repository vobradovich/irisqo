use tokio::{select, time};
use tracing::{debug, error, info, warn};

use crate::{
    db,
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
        if self.app_state.scheduler_options.is_none() {
            warn!({ instance_id = self.app_state.instance_id }, "disabled");
            return Ok(());
        }
        info!({ instance_id = self.app_state.instance_id }, "start");
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
                error!(
                    { instance_id = self.app_state.instance_id },
                    "error {}", err
                );
            }
            select!(
                _ = interval.tick() => {},
                _ = self.app_state.shutdown_token.cancelled() => {}
            );
        }
        info!({ instance_id = self.app_state.instance_id }, "stop");
        db::instances::kill(&self.app_state.pool, &self.app_state.instance_id).await?;
        Ok(())
    }

    async fn tick(&self) -> Result<(), Error> {
        debug!({ instance_id = self.app_state.instance_id }, "tick");
        db::instances::live(&self.app_state.pool, &self.app_state.instance_id).await?;
        let expired = db::instances::kill_expired(&self.app_state.pool, Duration::from_secs(30)).await?;
        let enqueued = db::jobqueue::enqueue_scheduled(&self.app_state.pool).await?;
        debug!({ instance_id = self.app_state.instance_id, enqueued = enqueued, expired = expired }, "db::jobs::enqueue_scheduled");
        Ok(())
    }
}
