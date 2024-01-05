use crate::{
    features::results::job_result::{JobResultRow, JobResultType},
    models::Error,
};
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

pub async fn processed(
    pool: &Pool<Postgres>,
    job_id: i64,
    job_result: JobResult,
) -> Result<u64, Error> {
    const SQL: &str = "WITH a AS (
        DELETE FROM enqueued WHERE id = $1 RETURNING id, retry, instance_id
    ), hist AS (
        INSERT INTO history SELECT id, retry, instance_id, now() as at, $2::history_status as status FROM a RETURNING id
    )
    INSERT INTO processed SELECT id, retry, instance_id, now() as at, $2::processed_status as status, $3 as meta, $4 as headers, $5 as body FROM a RETURNING id";
    let body: Option<&[u8]> = match job_result.body.is_empty() {
        true => None,
        false => Some(job_result.body.as_ref()),
    };
    let status = match job_result.meta.result {
        JobResultType::Timeout | JobResultType::Error { .. } => "failed",
        JobResultType::Cancelled => "cancelled",
        JobResultType::Http(ref meta) if meta.status_code.is_client_error() || meta.status_code.is_server_error() => "failed",
        _ => "completed",
    };
    let res = sqlx::query(SQL)
        .bind(job_id)
        .bind(status)
        .bind(Json(job_result.meta))
        .bind(Json(job_result.headers))
        .bind(body)
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}
