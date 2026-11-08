use std::time::Duration;

use tokio::{select, time};
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

use crate::{features, models::AppState};

/// Must stay well below [`INSTANCE_EXPIRE`].
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
/// An instance without a heartbeat for this long is dead: its jobs are released.
pub const INSTANCE_EXPIRE: Duration = Duration::from_secs(30);

/// What this process runs: the server and the worker always, the rest when enabled.
fn roles(app_state: &AppState) -> Vec<&'static str> {
    let mut roles = vec!["http", "worker"];
    if let Some(options) = &app_state.scheduler_options {
        roles.push("scheduler");
        if options.retention_hours > 0 {
            roles.push("retention");
        }
    }
    roles
}

/// Keeps the instance alive until `stop` is cancelled.
///
/// It has its own token (not `shutdown_token`): the heartbeat must go on while the
/// workers finish their jobs after a shutdown signal, otherwise another instance
/// would declare this one dead and run the same jobs again.
pub async fn run(app_state: &AppState, stop: CancellationToken) {
    let instance_id = &app_state.instance_id;
    info!({ instance_id }, "start");
    let mut interval = time::interval(HEARTBEAT_INTERVAL);
    interval.set_missed_tick_behavior(time::MissedTickBehavior::Delay);
    loop {
        select!(
            biased;
            () = stop.cancelled() => break,
            _ = interval.tick() => {},
        );
        _ = features::instances::live(&app_state.pool, instance_id, &roles(app_state))
            .await
            .inspect_err(|err| error!({ instance_id }, "instances::live error {:?}", err));
    }
    info!({ instance_id }, "stop");
}
