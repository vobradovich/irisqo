use crate::{
    features::results::job_result::JobResultRow,
    models::{Error, JobEntry},
};
use sqlx::{Pool, Postgres, types::Json};

use super::JobResult;

pub async fn get_by_id(pool: &Pool<Postgres>, job_id: i64) -> Result<Option<JobResultRow>, Error> {
    const SQL: &str = "SELECT * FROM processed WHERE id = $1";
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
        SELECT * FROM UNNEST($1::bigint[], $2::int[], $3::text[], $4::jsonb[], $5::jsonb[], $6::bytea[])
            AS t(id, retry, status, meta, headers, body)
    ), a AS (
        DELETE FROM enqueued AS e USING input AS i
        WHERE e.id = i.id AND e.retry = i.retry AND e.instance_id = $7
        RETURNING e.id
    ), hist AS (
        INSERT INTO history(id, retry, instance_id, status)
        SELECT i.id, i.retry, $7, i.status::history_status FROM input AS i INNER JOIN a ON a.id = i.id
    )
    INSERT INTO processed(id, retry, instance_id, status, meta, headers, body)
    SELECT i.id, i.retry, $7, i.status::processed_status, i.meta, i.headers, i.body
    FROM input AS i INNER JOIN a ON a.id = i.id";
    if items.is_empty() {
        return Ok(0);
    }
    let mut ids = Vec::with_capacity(items.len());
    let mut retries = Vec::with_capacity(items.len());
    let mut statuses = Vec::with_capacity(items.len());
    let mut metas = Vec::with_capacity(items.len());
    let mut headers = Vec::with_capacity(items.len());
    let mut bodies = Vec::with_capacity(items.len());
    for (entry, result) in items {
        ids.push(entry.id);
        retries.push(entry.retry);
        statuses.push(result.processed_status());
        metas.push(Json(&result.meta));
        headers.push(Json(&result.headers));
        bodies.push((!result.body.is_empty()).then_some(result.body.as_ref()));
    }
    let res = sqlx::query(SQL)
        .bind(ids)
        .bind(retries)
        .bind(statuses)
        .bind(metas)
        .bind(headers)
        .bind(bodies)
        .bind(instance_id)
        .execute(executor)
        .await?;
    Ok(res.rows_affected())
}
