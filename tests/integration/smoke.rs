use hyper::{Method, StatusCode};
use irisqo::{
    db,
    models::{JobCreate, JobMeta},
};
use sqlx::PgPool;

use crate::common;

#[sqlx::test(migrations = "./sql/migrations")]
async fn job_without_sink_completes(pool: PgPool) -> anyhow::Result<()> {
    let state = common::state(pool.clone());
    let job = JobCreate {
        meta: JobMeta {
            timeout: Some(1000),
            ..JobMeta::default()
        },
        ..JobCreate::default()
    };
    let created = db::jobqueue::create(&pool, job, &state.instance_id).await?;

    assert_eq!(common::run_enqueued(&state).await, 1);

    assert_eq!(
        common::processed_status(&pool, created.id).await.as_deref(),
        Some("completed")
    );
    Ok(())
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn http_job_is_queued_and_delivered(pool: PgPool) -> anyhow::Result<()> {
    let state = common::state(pool.clone());
    let addr = common::spawn_app(&state).await;

    let response = common::request(
        &state,
        Method::POST,
        &format!("http://{addr}/to/http://{addr}/echo"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let job_id: i64 = response
        .headers()
        .get("irisqo-job-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .expect("job-id header");

    assert_eq!(common::run_enqueued(&state).await, 1);

    assert_eq!(
        common::processed_status(&pool, job_id).await.as_deref(),
        Some("completed")
    );
    let history: Vec<String> =
        sqlx::query_scalar("SELECT kind FROM history WHERE id = $1 ORDER BY seq")
            .bind(job_id)
            .fetch_all(&pool)
            .await?;
    assert_eq!(history, ["enqueued", "claimed", "completed"]);
    Ok(())
}
