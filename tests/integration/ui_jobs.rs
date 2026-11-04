use hyper::{Method, StatusCode};
use irisqo::db::jobview::{self, JobQuery, JobStatus, ScheduleFilter, StatusFilter};
use sqlx::PgPool;

use crate::common;

/// A request without a body.
async fn send(
    state: &irisqo::AppState,
    method: Method,
    uri: &str,
    htmx: bool,
) -> (StatusCode, hyper::HeaderMap, String) {
    crate::ui::send(state, method, uri, htmx, "").await
}

async fn job_id(state: &irisqo::AppState, addr: std::net::SocketAddr, target: &str) -> i64 {
    let response = common::create(state, addr, target).await;
    assert_eq!(response.status(), StatusCode::CREATED, "{target}");
    common::header(&response, "job-id")
        .and_then(|v| v.parse().ok())
        .expect("job-id")
}

fn query(status: StatusFilter) -> JobQuery {
    JobQuery {
        status,
        limit: 50,
        ..JobQuery::default()
    }
}

async fn statuses(pool: &PgPool, q: &JobQuery) -> Vec<(i64, JobStatus)> {
    jobview::list(pool, q)
        .await
        .expect("list")
        .into_iter()
        .map(|j| (j.id, j.status))
        .collect()
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn list_shows_every_state(pool: PgPool) -> anyhow::Result<()> {
    let state = common::state(pool.clone());
    let addr = common::spawn_app(&state).await;
    let echo = format!("http://{addr}/echo");
    let completed = job_id(&state, addr, &format!("{echo}?_id=order-7731")).await;
    let failed = job_id(&state, addr, &format!("{echo}?status=404")).await;
    let retrying = job_id(
        &state,
        addr,
        &format!("{echo}?status=503&_retry=3|fixed|60"),
    )
    .await;
    common::run_enqueued(&state).await;
    let scheduled = job_id(&state, addr, &format!("{echo}?_delay=600")).await;
    let processing = job_id(&state, addr, &format!("{echo}?n=1")).await;
    assert_eq!(common::fetch(&state, 10).await.len(), 1);
    let enqueued = job_id(&state, addr, &format!("{echo}?n=2")).await;

    let all = statuses(&pool, &query(StatusFilter::All)).await;
    let expected = [
        (enqueued, JobStatus::Enqueued),
        (processing, JobStatus::Processing),
        (scheduled, JobStatus::Scheduled),
        (retrying, JobStatus::Retrying),
        (failed, JobStatus::Failed),
        (completed, JobStatus::Completed),
    ];
    assert_eq!(all, expected, "newest first, one state each");
    let active: Vec<i64> = statuses(&pool, &query(StatusFilter::Active))
        .await
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    assert_eq!(active, [enqueued, processing, scheduled, retrying]);
    assert_eq!(
        statuses(&pool, &query(StatusFilter::Only(JobStatus::Failed))).await,
        [(failed, JobStatus::Failed)]
    );

    // search: external id, id, a part of the URL
    for (search, expected) in [
        ("order-7731", vec![completed]),
        (&*failed.to_string(), vec![failed]),
        ("status=503", vec![retrying]),
    ] {
        let q = JobQuery {
            search: Some(search.to_owned()),
            ..query(StatusFilter::All)
        };
        let found: Vec<i64> = statuses(&pool, &q)
            .await
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert_eq!(found, expected, "{search}");
    }
    // keyset paging and the schedule filter
    let q = JobQuery {
        before: Some(scheduled),
        limit: 2,
        ..query(StatusFilter::All)
    };
    assert_eq!(statuses(&pool, &q).await.len(), 2);
    let q = JobQuery {
        schedule: ScheduleFilter::Scheduled,
        ..query(StatusFilter::All)
    };
    assert!(statuses(&pool, &q).await.is_empty());
    Ok(())
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn cancel_and_retry_through_the_api(pool: PgPool) -> anyhow::Result<()> {
    let state = common::state(pool.clone());
    let addr = common::spawn_app(&state).await;
    let api = |id: i64, action: &str| format!("http://{addr}/api/v1/jobs/{id}/{action}");
    let pending = job_id(&state, addr, &format!("http://{addr}/echo?_delay=600")).await;

    assert_eq!(
        send(&state, Method::POST, &api(pending, "retry"), false)
            .await
            .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        send(&state, Method::POST, &api(pending, "cancel"), false)
            .await
            .0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        common::processed_status(&pool, pending).await.as_deref(),
        Some("cancelled")
    );
    assert_eq!(
        common::history(&pool, pending)
            .await
            .pop()
            .map(|h| h.0)
            .as_deref(),
        Some("cancelled")
    );
    assert_eq!(
        send(&state, Method::POST, &api(pending, "cancel"), false)
            .await
            .0,
        StatusCode::CONFLICT
    );

    // a processed job runs again, with the next retry number
    assert_eq!(
        send(&state, Method::POST, &api(pending, "retry"), false)
            .await
            .0,
        StatusCode::NO_CONTENT
    );
    let retry: i32 = sqlx::query_scalar("SELECT retry FROM enqueued WHERE id = $1")
        .bind(pending)
        .fetch_one(&pool)
        .await?;
    assert_eq!(retry, 1);
    assert_eq!(common::run_enqueued(&state).await, 1);
    assert_eq!(
        common::processed_status(&pool, pending).await.as_deref(),
        Some("completed")
    );

    assert_eq!(
        send(&state, Method::POST, &api(999_999, "cancel"), false)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    Ok(())
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn schedule_pause_resume_and_one_chain(pool: PgPool) -> anyhow::Result<()> {
    let state = common::state(pool.clone());
    let addr = common::spawn_app(&state).await;
    let response = common::create(&state, addr, &format!("http://{addr}/echo?_interval=60")).await;
    let schedule_id = common::header(&response, "schedule-id").expect("schedule-id");
    let api = |action: &str| format!("http://{addr}/api/v1/schedules/{schedule_id}/{action}");

    // pause: inactive, the pending run is cancelled
    assert_eq!(
        send(&state, Method::POST, &api("pause"), false).await.0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        common::count(&pool, "SELECT count(*) FROM scheduled").await,
        0
    );
    assert_eq!(
        common::count(
            &pool,
            "SELECT count(*) FROM processed WHERE status = 'cancelled'"
        )
        .await,
        1
    );
    // resume: the next run is scheduled
    assert_eq!(
        send(&state, Method::POST, &api("resume"), false).await.0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        common::count(&pool, "SELECT count(*) FROM scheduled").await,
        1
    );
    assert_eq!(
        common::count(&pool, "SELECT count(*) FROM schedules WHERE inactive").await,
        0
    );
    // resuming again does not add a run
    send(&state, Method::POST, &api("resume"), false).await;
    assert_eq!(
        common::count(&pool, "SELECT count(*) FROM scheduled").await,
        1
    );

    // run it: the next run is cloned; then retrying the old run does not clone again
    common::enqueue_all_scheduled(&state).await;
    common::run_enqueued(&state).await;
    assert_eq!(
        common::count(&pool, "SELECT count(*) FROM scheduled").await,
        1
    );
    let old: i64 = sqlx::query_scalar("SELECT id FROM processed WHERE status = 'completed'")
        .fetch_one(&pool)
        .await?;
    let retry = format!("http://{addr}/api/v1/jobs/{old}/retry");
    assert_eq!(
        send(&state, Method::POST, &retry, false).await.0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(common::run_enqueued(&state).await, 1);
    assert_eq!(
        common::count(&pool, "SELECT count(*) FROM scheduled").await,
        1,
        "one chain only"
    );
    Ok(())
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn ui_pages_and_actions(pool: PgPool) -> anyhow::Result<()> {
    let state = common::state(pool.clone());
    let addr = common::spawn_app(&state).await;
    let ui = |path: &str| format!("http://{addr}{path}");
    let pending = job_id(&state, addr, &format!("http://{addr}/echo?_delay=600")).await;
    let response = common::create(&state, addr, &format!("http://{addr}/echo?_interval=60")).await;
    let schedule_id = common::header(&response, "schedule-id").expect("schedule-id");

    let (status, _, html) = send(&state, Method::GET, &ui("/ui/jobs"), false).await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains(&format!("id=\"job-{pending}\"")));
    assert!(html.contains("hx-trigger=\"every 2s\"") && html.contains("hx-swap=\"outerMorph\""));
    let (_, _, html) = send(&state, Method::GET, &ui("/ui/jobs?live=0"), false).await;
    assert!(!html.contains("hx-trigger=\"every 2s\""), "paused");
    let (_, _, html) = send(
        &state,
        Method::GET,
        &ui("/ui/jobs/live?status=scheduled"),
        true,
    )
    .await;
    assert!(html.starts_with("<div id=\"jobs-live\""));

    let (status, _, html) = send(
        &state,
        Method::GET,
        &ui(&format!("/ui/jobs/{pending}")),
        false,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("Cancel") && !html.contains("Retry now"));
    assert_eq!(
        send(&state, Method::GET, &ui("/ui/jobs/999999"), false)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    let (status, _, html) = send(
        &state,
        Method::GET,
        &ui(&format!("/ui/jobs/{pending}/history")),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("status scheduled"));

    // htmx gets the page partial with a notice, a plain form a redirect
    let (status, _, html) = send(
        &state,
        Method::POST,
        &ui(&format!("/ui/jobs/{pending}/cancel")),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        html.starts_with("<div id=\"job\"")
            && html.contains("Cancelled.")
            && html.contains("Retry now")
    );
    let (status, headers, _) = send(
        &state,
        Method::POST,
        &ui(&format!("/ui/jobs/{pending}/retry")),
        false,
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(
        headers.get("location").map(|v| v.as_bytes()),
        Some(format!("/ui/jobs/{pending}").as_bytes())
    );
    let (status, _, html) = send(
        &state,
        Method::POST,
        &ui(&format!("/ui/jobs/{pending}/retry")),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "already enqueued again");
    assert!(html.contains("Not possible any more"));
    let (status, headers, _) = send(
        &state,
        Method::DELETE,
        &ui(&format!("/ui/jobs/{pending}")),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers.get("hx-redirect").map(|v| v.as_bytes()),
        Some(&b"/ui/jobs"[..])
    );
    assert_eq!(
        common::count(
            &pool,
            "SELECT count(*) FROM jobs WHERE id IN (SELECT id FROM enqueued)"
        )
        .await,
        0
    );

    let (_, _, html) = send(&state, Method::GET, &ui("/ui/schedules"), false).await;
    assert!(html.contains(&schedule_id) && html.contains("every 1 min"));
    let (status, _, html) = send(
        &state,
        Method::POST,
        &ui(&format!("/ui/schedules/{schedule_id}/pause")),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.starts_with("<div id=\"schedules\"") && html.contains("Resume"));
    Ok(())
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn instances_page_shows_heartbeats_and_work(pool: PgPool) -> anyhow::Result<()> {
    let state = common::state(pool.clone());
    let addr = common::spawn_app(&state).await;
    irisqo::features::instances::live(&pool, &state.instance_id).await?;
    sqlx::query(
        "INSERT INTO instances(instance_id, last_at, status) VALUES
            ('quiet-host:01JC0000000000000000000000', now() - interval '1 minute', 'live'),
            ('old-host:01JB0000000000000000000000', now() - interval '2 hours', 'dead')",
    )
    .execute(&pool)
    .await?;
    let ids = common::insert_enqueued(&pool, 2).await;
    sqlx::query("UPDATE enqueued SET instance_id = $1, lock_at = now() WHERE id = ANY($2)")
        .bind(&state.instance_id)
        .bind(&ids)
        .execute(&pool)
        .await?;

    let (status, _, html) = send(
        &state,
        Method::GET,
        &format!("http://{addr}/ui/instances"),
        false,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("aria-current=\"page\">Instances</a>"));
    let (host, _) = state.instance_id.rsplit_once(':').expect("host:ulid");
    let this = html.find(host).expect("this instance");
    let quiet = html.find("quiet-host").expect("stale instance");
    let old = html.find("old-host").expect("dead instance");
    assert!(this.min(quiet) < old, "live instances first");
    assert!(html.contains("this server"));
    assert!(html.contains("<span class=\"status running\"><span>Live</span>"));
    assert!(html.contains("<span class=\"status stale\"><span>No heartbeat</span>"));
    assert!(html.contains("<span class=\"status stopped\"><span>Stopped</span>"));
    assert!(
        html.contains("<td class=\"num mono\">2</td>"),
        "jobs it holds"
    );

    let (status, _, html) = send(
        &state,
        Method::GET,
        &format!("http://{addr}/ui/instances/table"),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.starts_with("<div id=\"instances\"") && html.contains("hx-trigger=\"every 5s\""));
    Ok(())
}
