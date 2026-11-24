use tokio::{select, time};
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

use crate::{
    db, features,
    models::{AppState, HEARTBEAT_INTERVAL},
};

/// Keeps the instance alive and renews the leases of the jobs it holds until `stop` is cancelled.
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
        _ = features::instances::live(
            &app_state.pool,
            instance_id,
            &app_state.options.effective_roles().names(),
        )
        .await
        .inspect_err(|err| error!({ instance_id }, "instances::live error {:?}", err));
        let (ids, attempts) = app_state.held.columns();
        if !ids.is_empty() {
            _ = db::jobqueue::renew_leases(&app_state.pool, instance_id, &ids, &attempts)
                .await
                .inspect_err(|err| error!({ instance_id }, "renew_leases error {:?}", err));
        }
    }
    info!({ instance_id }, "stop");
}
