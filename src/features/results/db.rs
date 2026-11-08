use crate::{
    features::results::job_result::JobResultRow,
    models::{Error, JobEntry},
};
use sqlx::{Pool, Postgres, types::Json};

use super::JobResult;

pub async fn get_by_id(pool: &Pool<Postgres>, job_id: i64) -> Result<Option<JobResultRow>, Error> {
    const SQL: &str = "SELECT id, outcome AS meta, headers, body FROM results WHERE id = $1";
    let job = sqlx::query_as::<_, JobResultRow>(SQL)
        .bind(job_id)
        .fetch_optional(pool)
        .await?;
    Ok(job)
}

/// Completes the current attempt (`instance_id` + `retry`) of a job;
/// returns 0 affected rows when the lock is not ours any more.
pub async fn processed<'e, E>(
    executor: E,
    entry: JobEntry,
    instance_id: &str,
    job_result: &JobResult,
) -> Result<u64, Error>
where
    E: sqlx::Executor<'e, Database = Postgres>,
{
    processed_many(executor, &[(entry, job_result)], instance_id).await
}

/// Completes many jobs in one statement; jobs whose current attempt is not
/// (`instance_id` + `retry`) any more are skipped. Returns the number of completed jobs.
pub async fn processed_many<'e, E>(
    executor: E,
    items: &[(JobEntry, &JobResult)],
    instance_id: &str,
) -> Result<u64, Error>
where
    E: sqlx::Executor<'e, Database = Postgres>,
{
    const SQL: &str = "
    WITH input AS (
        SELECT * FROM UNNEST($1::bigint[], $2::int[], $3::text[], $4::jsonb[], $5::jsonb[], $6::bytea[], $8::text[])
            AS t(id, retry, status, meta, headers, body, message)
    ), a AS (
        DELETE FROM claimed AS c USING input AS i
        WHERE c.id = i.id AND c.retry = i.retry AND c.instance_id = $7
        RETURNING c.id
    ), hist AS (
        INSERT INTO history(id, retry, kind, instance_id, message)
        SELECT i.id, i.retry, i.status, $7, i.message FROM input AS i INNER JOIN a ON a.id = i.id
    ), p AS (
        INSERT INTO processed(id, retry, instance_id, status)
        SELECT i.id, i.retry, $7, i.status FROM input AS i INNER JOIN a ON a.id = i.id
        RETURNING id
    )
    INSERT INTO results(id, outcome, headers, body)
    SELECT i.id, i.meta, i.headers, i.body FROM input AS i INNER JOIN p ON p.id = i.id";
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
    for (entry, result) in items {
        ids.push(entry.id);
        retries.push(entry.retry);
        statuses.push(result.processed_status());
        metas.push(Json(&result.meta));
        headers.push(Json(&result.headers));
        bodies.push((!result.body.is_empty()).then_some(result.body.as_ref()));
        messages.push(result.history_message());
    }
    let res = sqlx::query(SQL)
        .bind(ids)
        .bind(retries)
        .bind(statuses)
        .bind(metas)
        .bind(headers)
        .bind(bodies)
        .bind(instance_id)
        .bind(messages)
        .execute(executor)
        .await?;
    Ok(res.rows_affected())
}
