use crate::{features::results::job_result::JobResultRow, models::Error};
use sqlx::{types::Json, Pool, Postgres};

use super::JobResult;

pub async fn get_by_id(pool: &Pool<Postgres>, job_id: i64) -> Result<Option<JobResultRow>, Error> {
    const SQL: &str = "SELECT * FROM processed WHERE id = $1";
    let job = sqlx::query_as::<_, JobResultRow>(SQL)
        .bind(job_id)
        .fetch_optional(pool)
        .await?;
    Ok(job)
}

pub async fn complete(
    pool: &Pool<Postgres>,
    job_id: i64,
    job_result: JobResult,
) -> Result<u64, Error> {
    const SQL: &str = "WITH a AS (
        DELETE FROM enqueued WHERE id = $1 RETURNING id, retry, instance_id
    )
    INSERT INTO processed SELECT id, retry, instance_id, now() as at, 'completed' as status, $2 as meta, $3 as headers, $4 as body FROM a RETURNING id";
    let body: Option<&[u8]> = match job_result.body.is_empty() {
        true => None,
        false => Some(job_result.body.as_ref()),
    };
    let res = sqlx::query(SQL)
        .bind(job_id)
        .bind(Json(job_result.meta))
        .bind(Json(job_result.headers))
        .bind(body)
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}

pub async fn fail(pool: &Pool<Postgres>, job_id: i64, job_result: JobResult) -> Result<u64, Error> {
    const SQL: &str = "WITH a AS (
        DELETE FROM enqueued WHERE id = $1 RETURNING id, retry, instance_id
    )
    INSERT INTO processed SELECT id, retry, instance_id, now() as at, 'failed' as status, $2 as meta, $3 as headers, $4 as body FROM a RETURNING id";
    let body: Option<&[u8]> = match job_result.body.is_empty() {
        true => None,
        false => Some(job_result.body.as_ref()),
    };
    let res = sqlx::query(SQL)
        .bind(job_id)
        .bind(Json(job_result.meta))
        .bind(Json(job_result.headers))
        .bind(body)
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}
