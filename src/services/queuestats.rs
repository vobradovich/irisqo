use std::time::Duration;

use opentelemetry::KeyValue;
use tokio::{select, time};
use tracing::{error, info};

use crate::{db, models::AppState, otel::metrics::metrics};

/// `count(*)` over the queue tables: often enough for a dashboard, rare enough for the db.
const STATS_INTERVAL: Duration = Duration::from_secs(15);

/// Records `irisqo.queue.depth` and `irisqo.queue.lag` until shutdown.
pub async fn run(app_state: &AppState) {
    let instance_id = &app_state.instance_id;
    info!({ instance_id }, "start");
    let mut interval = time::interval(STATS_INTERVAL);
    interval.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    loop {
        select!(
            biased;
            () = app_state.shutdown_token.cancelled() => break,
            _ = interval.tick() => {},
        );
        record(app_state).await;
    }
    info!({ instance_id }, "stop");
}

pub async fn record(app_state: &AppState) {
    let Ok(stats) = db::stats::queue_stats(&app_state.pool)
        .await
        .inspect_err(|err| {
            error!(
                { instance_id = app_state.instance_id },
                "queue_stats error {:?}", err
            )
        })
    else {
        return;
    };
    let metrics = metrics();
    for (state, count) in [
        ("scheduled", stats.scheduled),
        ("enqueued", stats.enqueued),
        ("processing", stats.processing),
    ] {
        metrics.queue_depth.record(
            u64::try_from(count).unwrap_or_default(),
            &[KeyValue::new("state", state)],
        );
    }
    metrics.queue_lag.record(stats.lag, &[]);
}
