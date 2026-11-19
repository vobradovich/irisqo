use crate::{
    features::results::job_result::JobResultRow,
    models::{Error, JobCreate, JobEntry},
};
use sqlx::{Pool, Postgres, types::Json};

use super::JobResult;

pub async fn get_by_id(pool: &Pool<Postgres>, job_id: i64) -> Result<Option<JobResultRow>, Error> {
    // a cancelled job has no response: its result is derived, not stored
    const SQL: &str = "
    SELECT id, coalesce(outcome, '{\"result\": \"cancelled\"}'::jsonb) AS meta, headers, body
    FROM processed
    WHERE id = $1 AND (outcome IS NOT NULL OR status = 'cancelled')";
    let job = sqlx::query_as::<_, JobResultRow>(SQL)
        .bind(job_id)
        .fetch_optional(pool)
        .await?;
    Ok(job)
}

/// Completes the current attempt (`instance_id` + `attempt`) of a job;
/// returns 0 affected rows when the lock is not ours any more. The callback is written with the
/// outcome, in the same statement.
pub async fn processed<'e, E>(
    executor: E,
    entry: JobEntry,
    instance_id: &str,
    job_result: &JobResult,
    callback: Option<&JobCreate>,
) -> Result<u64, Error>
where
    E: sqlx::Executor<'e, Database = Postgres>,
{
    processed_many(executor, &[(entry, job_result, callback)], instance_id).await
}

/// Completes many jobs in one statement; jobs whose current attempt is not
/// (`instance_id` + `attempt`) any more are skipped, and make no callback. Returns the number of
/// completed jobs.
pub async fn processed_many<'e, E>(
    executor: E,
    items: &[(JobEntry, &JobResult, Option<&JobCreate>)],
    instance_id: &str,
) -> Result<u64, Error>
where
    E: sqlx::Executor<'e, Database = Postgres>,
{
    const SQL: &str = "
    WITH input AS (
        SELECT * FROM UNNEST($1::bigint[], $2::int[], $3::text[], $4::jsonb[], $5::jsonb[], $6::bytea[], $8::text[],
                             $9::jsonb[], $10::jsonb[], $11::bytea[], $12::text[])
            AS t(id, attempt, status, meta, headers, body, message, cb_meta, cb_headers, cb_body, cb_key)
    ), a AS (
        -- no join with `input`: with stale stats the planner expects one claimed row and rescans `input` per row
        DELETE FROM claimed AS c
        WHERE c.id = ANY($1::bigint[]) AND c.instance_id = $7
          AND c.attempt = ($2::int[])[array_position($1::bigint[], c.id)]
        RETURNING c.id
    ), hist AS (
        INSERT INTO history(id, attempt, kind, instance_id, message)
        SELECT i.id, i.attempt, i.status, $7, i.message FROM input AS i INNER JOIN a ON a.id = i.id
    ), p AS (
        INSERT INTO processed(id, attempt, instance_id, status, outcome, headers, body)
        SELECT i.id, i.attempt, $7, i.status, i.meta, i.headers, i.body
        FROM input AS i INNER JOIN a ON a.id = i.id
        RETURNING id
    ), cb AS (
        INSERT INTO jobs(meta, headers, body, key, parent_id)
        SELECT i.cb_meta, i.cb_headers, i.cb_body, i.cb_key, i.id
        FROM input AS i INNER JOIN a ON a.id = i.id
        WHERE i.cb_meta IS NOT NULL
        RETURNING id, parent_id
    ), cbe AS (
        INSERT INTO enqueued(id, flow_key, priority)
        SELECT cb.id, i.cb_meta->>'flow_key', coalesce((i.cb_meta->>'priority')::int, 0)
        FROM cb INNER JOIN input AS i ON i.id = cb.parent_id
    ), cbh AS (
        INSERT INTO history(id, attempt, kind, message)
        SELECT cb.id, 0, 'enqueued', 'callback of #' || cb.parent_id FROM cb
    )
    SELECT count(*) FROM p";
    if items.is_empty() {
        return Ok(0);
    }
    let mut ids = Vec::with_capacity(items.len());
    let mut retries = Vec::with_capacity(items.len());
    let mut statuses = Vec::with_capacity(items.len());
    let mut metas = Vec::with_capacity(items.len());
    let mut headers = Vec::with_capacity(items.len());
    let mut bodies = Vec::with_capacity(items.len());
    let mut messages = Vec::with_capacity(items.len());
    let mut cb_metas = Vec::with_capacity(items.len());
    let mut cb_headers = Vec::with_capacity(items.len());
    let mut cb_bodies = Vec::with_capacity(items.len());
    let mut cb_keys = Vec::with_capacity(items.len());
    for (entry, result, callback) in items {
        ids.push(entry.id);
        retries.push(entry.attempt);
        statuses.push(result.processed_status());
        metas.push(Json(&result.meta));
        headers.push(Json(&result.headers));
        bodies.push((!result.body.is_empty()).then_some(result.body.as_ref()));
        messages.push(result.history_message());
        cb_metas.push(callback.map(|c| Json(&c.meta)));
        cb_headers.push(callback.map(|c| Json(&c.headers)));
        cb_bodies.push(callback.and_then(|c| c.body_bytes()));
        cb_keys.push(callback.and_then(|c| c.key.as_deref()));
    }
    let affected = sqlx::query_scalar::<_, i64>(SQL)
        .bind(ids)
        .bind(retries)
        .bind(statuses)
        .bind(metas)
        .bind(headers)
        .bind(bodies)
        .bind(instance_id)
        .bind(messages)
        .bind(cb_metas)
        .bind(cb_headers)
        .bind(cb_bodies)
        .bind(cb_keys)
        .fetch_one(executor)
        .await?;
    Ok(u64::try_from(affected).unwrap_or_default())
}
