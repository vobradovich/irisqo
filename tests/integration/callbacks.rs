use std::{collections::HashMap, net::SocketAddr};

use hyper::{Method, StatusCode};
use irisqo::models::SigningKeys;
use sqlx::PgPool;

use crate::common;

const SIGNING_KEY: &str = "whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw";

/// A `/to/` target that has a callback to `/echo?headers=1`, which answers with the headers it got.
fn target(addr: SocketAddr, query: &str) -> String {
    let callback = format!("http%3A%2F%2F{addr}%2Fecho%3Fheaders%3D1");
    format!("http://{addr}/echo?{query}&_callback_url={callback}")
}

async fn job_id(state: &irisqo::AppState, addr: SocketAddr, target: &str) -> i64 {
    let response = common::request_with(
        state,
        Method::POST,
        &format!("http://{addr}/to/{target}"),
        &[("irisqo-callback-authorization", "Bearer cb-secret")],
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    common::header(&response, "irisqo-job-id")
        .and_then(|v| v.parse().ok())
        .expect("irisqo-job-id")
}

async fn callbacks(pool: &PgPool, parent: i64) -> Vec<i64> {
    sqlx::query_scalar("SELECT id FROM jobs WHERE parent_id = $1 ORDER BY id")
        .bind(parent)
        .fetch_all(pool)
        .await
        .expect("callbacks")
}

async fn key(pool: &PgPool, id: i64) -> String {
    sqlx::query_scalar::<_, Option<String>>("SELECT key FROM jobs WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("key")
        .expect("a callback has a key")
}

async fn stored_body(pool: &PgPool, id: i64) -> Vec<u8> {
    sqlx::query_scalar::<_, Option<Vec<u8>>>("SELECT body FROM jobs WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("body")
        .unwrap_or_default()
}

async fn webhook(pool: &PgPool, id: i64) -> serde_json::Value {
    serde_json::from_slice(&stored_body(pool, id).await).expect("webhook json")
}

/// The headers the receiver of a callback got (it was `/echo?headers=1`).
async fn received(pool: &PgPool, id: i64) -> HashMap<String, String> {
    let body: Vec<u8> = sqlx::query_scalar("SELECT body FROM processed WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("processed body");
    serde_json::from_slice(&body).expect("headers json")
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn a_webhook_on_completed(pool: PgPool) -> anyhow::Result<()> {
    let state = common::state(pool.clone());
    let addr = common::spawn_app(&state).await;
    let id = job_id(&state, addr, &target(addr, "_key=order-42")).await;

    assert_eq!(common::run_enqueued(&state).await, 1);
    let made = callbacks(&pool, id).await;
    assert_eq!(made.len(), 1);
    let callback = made[0];
    // the callback is told where it comes from; the job has no row of its own for it
    assert_eq!(
        common::history(&pool, callback).await[0],
        ("enqueued".into(), Some(format!("callback of #{id}")))
    );
    let history = common::history(&pool, id).await;
    assert!(
        history.iter().all(|(kind, _)| kind != "callback"),
        "{history:?}"
    );

    // the callback is a job: it runs and is processed
    assert_eq!(common::run_enqueued(&state).await, 1);
    assert_eq!(
        common::processed_status(&pool, callback).await.as_deref(),
        Some("completed")
    );
    // the headers of the standard, none of Irisqo's, and no signature without a key
    let got = received(&pool, callback).await;
    assert_eq!(got["webhook-id"], key(&pool, callback).await);
    assert!(got["webhook-id"].starts_with("cb_"));
    assert!(got["webhook-timestamp"].parse::<i64>().is_ok());
    assert!(!got.contains_key("webhook-signature"));
    assert!(got.keys().all(|name| !name.starts_with("irisqo-")));
    assert_eq!(got["authorization"], "Bearer cb-secret");
    assert_eq!(got["content-type"], "application/json");

    let body = webhook(&pool, callback).await;
    assert_eq!(body["type"], "irisqo.job.completed");
    assert!(body["timestamp"].is_string());
    assert_eq!(body["data"]["job"]["id"], id);
    assert_eq!(body["data"]["job"]["key"], "order-42");
    assert_eq!(body["data"]["result"]["meta"]["status_code"], 200);

    // a callback has no callback, and the job is unchanged
    assert_eq!(callbacks(&pool, callback).await.len(), 0);
    assert_eq!(common::count(&pool, "SELECT count(*) FROM jobs").await, 2);
    // the callbacks of a job are an API of their own
    let response = common::request(
        &state,
        Method::GET,
        &format!("http://{addr}/api/v1/jobs?parent_id={id}"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        common::text(response)
            .await
            .contains(&format!("\"parent_id\":{id}"))
    );
    Ok(())
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn a_webhook_is_signed_when_there_is_a_key(pool: PgPool) -> anyhow::Result<()> {
    let state = common::state_with(pool.clone(), &["--signing-key", SIGNING_KEY]);
    let addr = common::spawn_app(&state).await;
    let id = job_id(&state, addr, &target(addr, "x=1")).await;
    common::run_enqueued(&state).await;
    let callback = callbacks(&pool, id).await[0];
    common::run_enqueued(&state).await;

    let got = received(&pool, callback).await;
    let keys: SigningKeys = SIGNING_KEY.parse().expect("key");
    let expected = keys.sign(
        &key(&pool, callback).await,
        got["webhook-timestamp"].parse()?,
        &stored_body(&pool, callback).await,
    );
    assert_eq!(got["webhook-signature"], expected);
    assert!(expected.starts_with("v1,"));
    Ok(())
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn a_failed_job_and_a_redirect_make_a_webhook_and_a_relay_makes_none(
    pool: PgPool,
) -> anyhow::Result<()> {
    let state = common::state(pool.clone());
    let addr = common::spawn_app(&state).await;
    let failed = job_id(&state, addr, &target(addr, "status=500")).await;
    let redirected = job_id(&state, addr, &target(addr, "status=302&_retry=3")).await;
    let relay = job_id(
        &state,
        addr,
        &format!("{}&_callback_mode=relay", target(addr, "status=500")),
    )
    .await;

    assert_eq!(common::run_enqueued(&state).await, 3);
    // a 3xx is a failure that is not retried: it is processed at once
    assert_eq!(
        common::processed_status(&pool, redirected).await.as_deref(),
        Some("failed")
    );
    assert_eq!(callbacks(&pool, relay).await.len(), 0);
    for (job, status) in [(failed, 500), (redirected, 302)] {
        let made = callbacks(&pool, job).await;
        assert_eq!(made.len(), 1);
        let body = webhook(&pool, made[0]).await;
        assert_eq!(body["type"], "irisqo.job.failed");
        assert_eq!(body["data"]["result"]["meta"]["status_code"], status);
    }
    Ok(())
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn a_relay_sends_the_response_as_it_is(pool: PgPool) -> anyhow::Result<()> {
    let state = common::state(pool.clone());
    let addr = common::spawn_app(&state).await;
    let id = job_id(
        &state,
        addr,
        &format!(
            "{}&_callback_mode=relay",
            target(addr, "headers=1&_key=order-7")
        ),
    )
    .await;

    assert_eq!(common::run_enqueued(&state).await, 1);
    let made = callbacks(&pool, id).await;
    assert_eq!(made.len(), 1);
    let response: Vec<u8> = sqlx::query_scalar("SELECT body FROM processed WHERE id = $1")
        .bind(id)
        .fetch_one(&pool)
        .await?;
    assert_eq!(stored_body(&pool, made[0]).await, response);

    assert_eq!(common::run_enqueued(&state).await, 1);
    let got = received(&pool, made[0]).await;
    assert_eq!(got["irisqo-job-id"], made[0].to_string());
    assert_eq!(got["irisqo-job-key"], key(&pool, made[0]).await);
    assert_eq!(got["irisqo-attempt"], "0");
    assert_eq!(got["irisqo-parent-id"], id.to_string());
    assert_eq!(got["irisqo-parent-key"], "order-7");
    assert_eq!(got["irisqo-parent-status"], "200");
    assert_eq!(got["irisqo-event-type"], "irisqo.job.completed");
    assert!(got["irisqo-timestamp"].parse::<i64>().is_ok());
    assert!(
        !got.contains_key("irisqo-signature"),
        "no key, no signature"
    );
    assert!(!got.keys().any(|name| name.starts_with("webhook-")));
    Ok(())
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn a_relay_of_a_failure_sends_headers_only(pool: PgPool) -> anyhow::Result<()> {
    let state = common::state_with(pool.clone(), &["--signing-key", SIGNING_KEY]);
    let addr = common::spawn_app(&state).await;
    let query = "_callback_mode=relay&_callback_types=irisqo.job.failed,irisqo.job.cancelled";
    let id = job_id(
        &state,
        addr,
        &format!("{}&{query}", target(addr, "status=500")),
    )
    .await;
    let completed = job_id(&state, addr, &format!("{}&{query}", target(addr, "x=1"))).await;

    assert_eq!(common::run_enqueued(&state).await, 2);
    assert_eq!(callbacks(&pool, completed).await.len(), 0);
    let made = callbacks(&pool, id).await;
    assert_eq!(made.len(), 1);
    assert!(stored_body(&pool, made[0]).await.is_empty());

    assert_eq!(common::run_enqueued(&state).await, 1);
    let got = received(&pool, made[0]).await;
    assert_eq!(got["irisqo-event-type"], "irisqo.job.failed");
    assert_eq!(got["irisqo-parent-status"], "500");
    assert!(!got.contains_key("content-type"));
    // a relay is signed as a webhook is: the same recipe, the names of Irisqo
    let keys: SigningKeys = SIGNING_KEY.parse().expect("key");
    assert_eq!(
        got["irisqo-signature"],
        keys.sign(
            &got["irisqo-job-key"],
            got["irisqo-timestamp"].parse()?,
            b""
        )
    );
    Ok(())
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn cancelling_a_waiting_job_makes_a_callback(pool: PgPool) -> anyhow::Result<()> {
    let state = common::state(pool.clone());
    let addr = common::spawn_app(&state).await;
    let id = job_id(&state, addr, &target(addr, "_delay=600")).await;
    sqlx::query("UPDATE scheduled SET attempt = 3 WHERE id = $1")
        .bind(id)
        .execute(&pool)
        .await?;

    let response = common::request(
        &state,
        Method::POST,
        &format!("http://{addr}/api/v1/jobs/{id}/cancel"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let made = callbacks(&pool, id).await;
    assert_eq!(made.len(), 1);
    assert!(key(&pool, made[0]).await.starts_with("cb_"));
    let body = webhook(&pool, made[0]).await;
    assert_eq!(body["type"], "irisqo.job.cancelled");
    assert_eq!(body["data"]["job"]["attempt"], 3);
    assert_eq!(body["data"]["result"]["meta"]["result"], "cancelled");

    // a second cancel is a conflict and makes nothing
    let response = common::request(
        &state,
        Method::POST,
        &format!("http://{addr}/api/v1/jobs/{id}/cancel"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(callbacks(&pool, id).await.len(), 1);
    Ok(())
}

/// A run that its schedule takes back (declared again, paused) makes no callback: only a
/// user's cancel does, and the history says who it was. A meta that cannot be read does not
/// stop a cancel.
#[sqlx::test(migrations = "./sql/migrations")]
async fn only_a_cancel_by_a_user_makes_a_callback(pool: PgPool) -> anyhow::Result<()> {
    use irisqo::db::jobqueue::cancel;

    let state = common::state(pool.clone());
    let addr = common::spawn_app(&state).await;
    for (taken_back, message, made) in [
        (
            Some("replaced: the schedule was declared again"),
            "replaced: the schedule was declared again",
            0,
        ),
        (
            Some("the schedule was paused"),
            "the schedule was paused",
            0,
        ),
        (None, "cancelled by a user", 1),
    ] {
        let id = job_id(&state, addr, &target(addr, "_delay=600")).await;
        let mut conn = pool.acquire().await?;
        assert_eq!(cancel(&mut conn, id, "test", taken_back).await?, 1);
        drop(conn);
        assert_eq!(callbacks(&pool, id).await.len(), made, "{message}");
        assert_eq!(
            common::processed_status(&pool, id).await.as_deref(),
            Some("cancelled")
        );
        let said: String =
            sqlx::query_scalar("SELECT message FROM history WHERE id = $1 AND kind = 'cancelled'")
                .bind(id)
                .fetch_one(&pool)
                .await?;
        assert_eq!(said, message);
    }

    let id = job_id(&state, addr, &target(addr, "_delay=600")).await;
    sqlx::query(
        r#"UPDATE jobs SET meta = jsonb_set(meta, '{callback,url}', '"http://h/a b"') WHERE id = $1"#,
    )
    .bind(id)
    .execute(&pool)
    .await?;
    let mut conn = pool.acquire().await?;
    assert_eq!(cancel(&mut conn, id, "test", None).await?, 1);
    drop(conn);
    assert_eq!(callbacks(&pool, id).await.len(), 0);
    Ok(())
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn cancel_conflict_uses_one_connection(pool: PgPool) -> anyhow::Result<()> {
    use irisqo::db::jobqueue::{ActionOutcome, cancel_job};
    use std::time::Duration;

    let id = sqlx::query_scalar::<_, i64>("SELECT enqueue_v1('{\"url\":\"noop://x\"}')")
        .fetch_one(&pool)
        .await?;
    let single = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(2))
        .connect_with(pool.connect_options().as_ref().clone())
        .await?;

    assert!(matches!(
        cancel_job(&single, id, "test").await?,
        ActionOutcome::Done
    ));
    assert!(matches!(
        cancel_job(&single, id, "test").await?,
        ActionOutcome::Conflict
    ));
    Ok(())
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn a_manual_retry_makes_a_second_callback(pool: PgPool) -> anyhow::Result<()> {
    let state = common::state(pool.clone());
    let addr = common::spawn_app(&state).await;
    let id = job_id(&state, addr, &target(addr, "x=1")).await;
    assert_eq!(common::run_enqueued(&state).await, 1);
    // the callback, then the job again
    assert_eq!(common::run_enqueued(&state).await, 1);
    irisqo::db::jobqueue::requeue_job(&state.pool, id, &state.instance_id).await?;
    assert_eq!(common::run_enqueued(&state).await, 1);

    let made = callbacks(&pool, id).await;
    assert_eq!(made.len(), 2);
    let mut attempts = Vec::new();
    for callback in &made {
        attempts.push(webhook(&pool, *callback).await["data"]["job"]["attempt"].as_i64());
    }
    assert_eq!(attempts, [Some(0), Some(1)]);
    assert_ne!(key(&pool, made[0]).await, key(&pool, made[1]).await);
    Ok(())
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn a_callback_has_its_own_method_and_defaults(pool: PgPool) -> anyhow::Result<()> {
    let state = common::state(pool.clone());
    let addr = common::spawn_app(&state).await;
    let id = job_id(
        &state,
        addr,
        &format!(
            "{}&_callback_method=put&_retry=1&_timeout=1000",
            target(addr, "x=1")
        ),
    )
    .await;
    common::run_enqueued(&state).await;

    let callback = callbacks(&pool, id).await[0];
    let meta: serde_json::Value = sqlx::query_scalar("SELECT meta FROM jobs WHERE id = $1")
        .bind(callback)
        .fetch_one(&pool)
        .await?;
    assert_eq!(meta["method"], "PUT");
    // not the retries and the timeout of the job
    assert_eq!(meta["retry"]["policy"], "exponential");
    assert_eq!(meta["retry"]["count"], 10);
    assert_eq!(meta["timeout"], 30_000);
    assert_eq!(meta["callback_as"], "webhook");
    Ok(())
}

#[sqlx::test(migrations = "./sql/migrations")]
async fn callback_fields_are_checked(pool: PgPool) -> anyhow::Result<()> {
    let state = common::state(pool.clone());
    let addr = common::spawn_app(&state).await;
    for query in [
        "_callback_mode=relay",
        "_callback_retry=3",
        "_callback_types=irisqo.job.failed",
        "_callback_method=PUT",
        "_callback_url=http%3A%2F%2Fx.example&_callback_mode=final",
        "_callback_url=http%3A%2F%2Fx.example&_callback_method=GET",
        "_callback_url=http%3A%2F%2Fx.example&_callback_types=completed",
        "_callback_url=http%3A%2F%2Fx.example&_callback_types=",
        "_callback_url=ftp%3A%2F%2Fx.example",
    ] {
        let response = common::create(&state, addr, &format!("http://{addr}/echo?{query}")).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{query}");
    }
    // the names of the standard are irisqo's to set
    let response = common::request_with(
        &state,
        Method::POST,
        &format!("http://{addr}/to/http://{addr}/echo?_callback_url=http%3A%2F%2Fx.example"),
        &[("irisqo-callback-webhook-id", "spoofed")],
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    // the same checks in `enqueue_v1`
    for job in [
        r#"{"url":"noop://x","callback":{"mode":"relay"}}"#,
        r#"{"url":"noop://x","callback":"http://x.example"}"#,
        r#"{"url":"noop://x","callback":{"url":"ftp://x.example"}}"#,
        r#"{"url":"noop://x","callback":{"url":"http://x.example","mode":"final"}}"#,
        r#"{"url":"noop://x","callback":{"url":"http://x.example","method":"GET"}}"#,
        r#"{"url":"noop://x","callback":{"url":"http://x.example","types":["completed"]}}"#,
        r#"{"url":"noop://x","callback":{"url":"http://x.example","types":[]}}"#,
        r#"{"url":"noop://x","callback":{"types":["irisqo.job.failed"]}}"#,
        // the names irisqo sets, in any case, and ones that are not header names
        r#"{"url":"noop://x","callback":{"url":"http://x.example","headers":{"Irisqo-Parent-Id":"999"}}}"#,
        r#"{"url":"noop://x","callback":{"url":"http://x.example","headers":{"irisqo-event-type":"x"}}}"#,
        r#"{"url":"noop://x","callback":{"url":"http://x.example","headers":{"Webhook-Id":"x"}}}"#,
        r#"{"url":"noop://x","callback":{"url":"http://x.example","headers":{"host":"x"}}}"#,
        r#"{"url":"noop://x","callback":{"url":"http://x.example","headers":{"irisqo-forward-host":"x"}}}"#,
        r#"{"url":"noop://x","callback":{"url":"http://x.example","headers":{"irisqo-forward-irisqo-job-id":"x"}}}"#,
        r#"{"url":"noop://x","callback":{"url":"http://x.example","headers":{"irisqo-forward-":"x"}}}"#,
        r#"{"url":"noop://x","callback":{"url":"http://x.example","headers":{"":"x"}}}"#,
        r#"{"url":"noop://x","callback":{"url":"http://x.example","headers":{"x y":"x"}}}"#,
        r#"{"url":"noop://x","callback":{"url":"http://x.example","headers":{"x-own":"a\nb"}}}"#,
        r#"{"url":"noop://x","callback":{"url":"http://x.example","headers":{"X-Own":"a","x-own":"b"}}}"#,
        // a policy has its kind, a backoff its delay
        r#"{"url":"noop://x","callback":{"url":"http://x.example","retry":{"count":3}}}"#,
        r#"{"url":"noop://x","callback":{"url":"http://x.example","retry":{"policy":"fibonacci","count":3}}}"#,
        // a URL the server cannot read would make the whole job unreadable
        r#"{"url":"noop://x","callback":{"url":"http://x.example/<a>"}}"#,
        r#"{"url":"noop://x","callback":{"url":"http://[x.example"}}"#,
        r#"{"url":"noop://x","callback":{"url":"http://a@b@x.example"}}"#,
        r#"{"url":"http://x.example/<a>"}"#,
    ] {
        let res = sqlx::query_scalar::<_, i64>("SELECT enqueue_v1($1::jsonb)")
            .bind(job)
            .fetch_one(&pool)
            .await;
        assert!(res.is_err(), "{job}");
    }
    let id: i64 = sqlx::query_scalar(
        r#"SELECT enqueue_v1('{"url":"noop://x","callback":{"url":"http://x.example","method":"put","types":["irisqo.job.failed"],"retry":{"policy":"fixed","count":3},"headers":{"Irisqo-Forward-Authorization":"Bearer t","x-own":"a\tb"}}}'::jsonb)"#,
    )
    .fetch_one(&pool)
    .await?;
    let callback: serde_json::Value =
        sqlx::query_scalar("SELECT meta->'callback' FROM jobs WHERE id = $1")
            .bind(id)
            .fetch_one(&pool)
            .await?;
    assert_eq!(callback["mode"], "webhook");
    assert_eq!(callback["method"], "PUT");
    assert_eq!(callback["types"], serde_json::json!(["irisqo.job.failed"]));
    assert_eq!(callback["retry"]["count"], 3);
    assert_eq!(callback["headers"]["x-own"], "a\tb");
    Ok(())
}

/// No retries is a policy of its own (`count` 0), not the default of a callback.
#[sqlx::test(migrations = "./sql/migrations")]
async fn callback_retry_none_is_not_the_default(pool: PgPool) -> anyhow::Result<()> {
    let state = common::state(pool.clone());
    let addr = common::spawn_app(&state).await;
    async fn callback_meta(pool: &PgPool, id: i64) -> serde_json::Value {
        sqlx::query_scalar("SELECT meta->'callback' FROM jobs WHERE id = $1")
            .bind(id)
            .fetch_one(pool)
            .await
            .expect("callback")
    }
    let no_retries = |meta: &serde_json::Value| {
        serde_json::from_value::<irisqo::models::JobRetry>(meta["retry"].clone()).ok()
            == Some(irisqo::models::JobRetry::NONE)
    };

    // the JSON API
    let url = format!("http://{addr}/api/v1/jobs");
    let send_json = |job: &'static str| {
        common::send(
            &state,
            Method::POST,
            &url,
            &[("content-type", "application/json")],
            job,
        )
    };
    let response =
        send_json(
            r#"{"url":"noop://x","callback":{"url":"http://x.example","retry":{"policy":"fixed","count":0}}}"#,
        )
        .await;
    assert!(response.status().is_success());
    let id: i64 = common::header(&response, "irisqo-job-id")
        .and_then(|v| v.parse().ok())
        .expect("job id");
    assert!(no_retries(&callback_meta(&pool, id).await));
    // a policy that is not valid is refused, not replaced by the default
    let response = send_json(
        r#"{"url":"noop://x","callback":{"url":"http://x.example","retry":{"policy":"fixed"}}}"#,
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    // `/to/`
    let response = common::create(
        &state,
        addr,
        &format!("http://{addr}/echo?_callback_url=http%3A%2F%2Fx.example&_callback_retry=none"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let id: i64 = common::header(&response, "irisqo-job-id")
        .and_then(|v| v.parse().ok())
        .expect("job id");
    assert!(no_retries(&callback_meta(&pool, id).await));

    // `enqueue_v1`, and the default when the policy is not given
    let none: i64 = sqlx::query_scalar(
        r#"SELECT enqueue_v1('{"url":"noop://x","callback":{"url":"http://x.example","retry":{"policy":"fixed","count":0}}}'::jsonb)"#,
    )
    .fetch_one(&pool)
    .await?;
    assert!(no_retries(&callback_meta(&pool, none).await));
    let absent: i64 = sqlx::query_scalar(
        r#"SELECT enqueue_v1('{"url":"noop://x","callback":{"url":"http://x.example"}}'::jsonb)"#,
    )
    .fetch_one(&pool)
    .await?;
    assert!(callback_meta(&pool, absent).await.get("retry").is_none());
    Ok(())
}

/// `enqueue_v1` refuses the callback headers that cannot be sent, but a row written to `jobs` by
/// hand may have one (irisqo's own, a bad name or value): when the job ends, its callback is not
/// made, and the job is done all the same.
#[sqlx::test(migrations = "./sql/migrations")]
async fn a_callback_with_a_forged_header_is_not_made(pool: PgPool) -> anyhow::Result<()> {
    let state = common::state(pool.clone());
    let headers = |name: &str, value: &str| serde_json::json!({ name: value });
    let enqueue = |headers: serde_json::Value| {
        let job = serde_json::json!({
            "url":"noop://x",
            "callback": {"url": "http://x.example", "mode": "relay", "headers": headers},
        });
        sqlx::query_scalar::<_, i64>("SELECT enqueue_v1($1::jsonb)")
            .bind(job.to_string())
            .fetch_one(&pool)
    };
    let mut forged = Vec::new();
    for (name, value) in [
        ("Irisqo-Parent-Id", "999"),
        ("irisqo-event-type", "forged"),
        ("webhook-signature", "forged"),
        ("Host", "evil.example"),
        ("bad name", "1"),
        ("x-own", "bad\u{1}value"),
    ] {
        let id = enqueue(headers("x-own", "fine")).await?;
        sqlx::query(
            "UPDATE jobs SET meta = jsonb_set(meta, '{callback,headers}', $2) WHERE id = $1",
        )
        .bind(id)
        .bind(headers(name, value))
        .execute(&pool)
        .await?;
        forged.push(id);
    }
    let fine = enqueue(headers("x-own", "kept")).await?;

    assert_eq!(common::run_enqueued(&state).await, 7);
    for id in forged {
        assert_eq!(
            common::processed_status(&pool, id).await.as_deref(),
            Some("completed")
        );
        assert!(callbacks(&pool, id).await.is_empty(), "{id}");
    }
    assert_eq!(callbacks(&pool, fine).await.len(), 1);
    Ok(())
}

/// A chain through irisqo: `irisqo-forward-*` in the callback headers is for the next irisqo,
/// which forwards it to its target; the API shows its value hidden, as any other secret.
#[sqlx::test(migrations = "./sql/migrations")]
async fn a_callback_may_carry_the_forward_headers_of_a_chain(pool: PgPool) -> anyhow::Result<()> {
    let state = common::state(pool.clone());
    let addr = common::spawn_app(&state).await;
    let json = format!(
        r#"{{"url":"noop://x","callback":{{"url":"http://{addr}/to/http://{addr}/echo","mode":"relay",
            "headers":{{"Authorization":"Bearer irisqo","Irisqo-Forward-Authorization":"Bearer for-next"}}}}}}"#
    );
    let response = common::send(
        &state,
        Method::POST,
        &format!("http://{addr}/api/v1/jobs"),
        &[("content-type", "application/json")],
        &json,
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let id = common::header(&response, "irisqo-job-id")
        .and_then(|v| v.parse::<i64>().ok())
        .expect("job id");
    // the API shows the job without the values
    let shown = common::text(
        common::request(
            &state,
            Method::GET,
            &format!("http://{addr}/api/v1/jobs/{id}"),
        )
        .await,
    )
    .await;
    assert!(
        !shown.contains("for-next") && !shown.contains("Bearer irisqo"),
        "{shown}"
    );
    let stored: serde_json::Value =
        sqlx::query_scalar("SELECT meta->'callback'->'headers' FROM jobs WHERE id = $1")
            .bind(id)
            .fetch_one(&pool)
            .await?;
    assert_eq!(stored["irisqo-forward-authorization"], "Bearer for-next");

    // the callback is made with it, as the headers of the job
    assert_eq!(common::run_enqueued(&state).await, 1);
    let made = callbacks(&pool, id).await;
    assert_eq!(made.len(), 1);
    let headers: serde_json::Value = sqlx::query_scalar("SELECT headers FROM jobs WHERE id = $1")
        .bind(made[0])
        .fetch_one(&pool)
        .await?;
    assert_eq!(headers["irisqo-forward-authorization"], "Bearer for-next");
    Ok(())
}

/// The callbacks of a job come in pages, and the page of one says so when the job is gone.
#[sqlx::test(migrations = "./sql/migrations")]
async fn callbacks_are_paged_and_outlive_their_parent(pool: PgPool) -> anyhow::Result<()> {
    let state = common::state(pool.clone());
    let addr = common::spawn_app(&state).await;
    let id = job_id(&state, addr, &target(addr, "_delay=600")).await;
    sqlx::query(
        r#"INSERT INTO jobs(meta, parent_id)
           SELECT '{"sink":"noop","callback_as":"webhook"}'::jsonb, $1 FROM generate_series(1, 3)"#,
    )
    .bind(id)
    .execute(&pool)
    .await?;

    let page = |query: String| {
        let (state, url) = (
            &state,
            format!("http://{addr}/api/v1/jobs?parent_id={id}{query}"),
        );
        async move {
            let response = common::request(state, Method::GET, &url).await;
            assert_eq!(response.status(), StatusCode::OK);
            let jobs: Vec<serde_json::Value> =
                serde_json::from_str(&common::text(response).await).expect("json");
            jobs.iter()
                .map(|job| job["id"].as_i64().expect("id"))
                .collect::<Vec<_>>()
        }
    };
    let all = page(String::new()).await;
    assert_eq!(all.len(), 3);
    let first = page("&limit=2".into()).await;
    assert_eq!(first, all[..2]);
    let rest = page(format!("&limit=2&after={}", first[1])).await;
    assert_eq!(rest, all[2..]);

    // the parent is deleted: its callbacks are still callbacks, and not direct jobs
    let response = common::request(
        &state,
        Method::DELETE,
        &format!("http://{addr}/api/v1/jobs/{id}"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    // the page of the job tells that it is a callback, whose job is gone
    let response = common::request(
        &state,
        Method::GET,
        &format!("http://{addr}/ui/jobs/{}", all[0]),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        common::text(response)
            .await
            .contains("a deleted job · webhook")
    );
    Ok(())
}
