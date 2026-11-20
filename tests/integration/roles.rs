//! What an instance serves and writes depends on its roles.

use std::time::Duration;

use hyper::Method;
use irisqo::services::heartbeat;
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;

use crate::common;

/// The status of `GET path` on an instance started with `--roles`.
async fn status(pool: &PgPool, roles: &str, path: &str) -> u16 {
    let state = common::state_with(pool.clone(), &["--roles", roles]);
    let addr = common::spawn_app(&state).await;
    common::request(&state, Method::GET, &format!("http://{addr}{path}"))
        .await
        .status()
        .as_u16()
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn the_routes_follow_the_roles(pool: PgPool) -> anyhow::Result<()> {
    // (roles, path, status): the probes are always there, the rest only with their role
    let cases = [
        ("worker", "/live", 200),
        ("worker", "/ready", 200),
        ("worker", "/api/v1/instances", 404),
        ("worker", "/ui/jobs", 404),
        ("worker", "/", 404),
        ("scheduler,retention", "/live", 200),
        ("scheduler,retention", "/api/v1/instances", 404),
        ("api", "/live", 200),
        ("api", "/api/v1/instances", 200),
        ("api", "/ui/jobs", 404),
        ("api", "/", 200),
        ("ui", "/live", 200),
        ("ui", "/ui/jobs", 200),
        ("ui", "/api/v1/instances", 404),
        ("ui", "/", 200),
        ("api,ui,worker", "/api/v1/instances", 200),
        ("api,ui,worker", "/ui/jobs", 200),
    ];
    for (roles, path, expected) in cases {
        assert_eq!(status(&pool, roles, path).await, expected, "{roles} {path}");
    }
    Ok(())
}

/// The heartbeat writes what the instance really runs: `retention` needs a retention time.
#[sqlx::test(migrations = "./sql/migrations")]
async fn the_heartbeat_writes_the_effective_roles(pool: PgPool) -> anyhow::Result<()> {
    for (args, expected) in [
        (&["--roles", "worker,ui"][..], vec!["ui", "worker"]),
        (
            &["--roles", "scheduler,retention", "--retention-hours", "0"][..],
            vec!["scheduler"],
        ),
        (
            &[][..],
            vec!["api", "ui", "worker", "scheduler", "retention"],
        ),
    ] {
        let state = common::state_with(pool.clone(), args);
        let stop = CancellationToken::new();
        let beat = tokio::spawn({
            let (state, stop) = (state.clone(), stop.clone());
            async move { heartbeat::run(&state, stop).await }
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        stop.cancel();
        beat.await?;

        let roles: Vec<String> =
            sqlx::query_scalar("SELECT roles FROM instances WHERE instance_id = $1")
                .bind(&state.instance_id)
                .fetch_one(&pool)
                .await?;
        assert_eq!(roles, expected, "{args:?}");
    }
    Ok(())
}
