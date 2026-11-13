use sqlx::{Pool, Postgres};

use super::{FlowCreate, FlowListRow, FlowRow, UndeclaredFlowRow};
use crate::models::Error;

/// A page of the declared flows by name, with their jobs counted: `waiting` stops at 101 (an
/// index range of at most 101 entries per flow), `processing` is at most the concurrency.
/// `after` is the last name of the previous page.
pub async fn list(
    pool: &Pool<Postgres>,
    after: Option<&str>,
    limit: i64,
    offset: i64,
) -> Result<Vec<FlowListRow>, Error> {
    const SQL: &str = "
    SELECT flow_id, concurrency, rate, rate_period, paused, next_at,
           (SELECT count(*) FROM (SELECT 1 FROM enqueued e WHERE e.flow_key = flows.flow_id LIMIT 101) w) AS waiting,
           (SELECT count(*) FROM claimed c WHERE c.flow_key = flows.flow_id) AS processing,
           CASE WHEN rate IS NULL THEN NULL
                ELSE flow_rate_free_v1(rate, rate_period, next_at) END AS rate_left
    FROM flows
    WHERE flow_id > coalesce($1::text, '')
    ORDER BY flow_id
    LIMIT $2 OFFSET $3";
    let rows = sqlx::query_as::<_, FlowListRow>(SQL)
        .bind(after)
        .bind(limit)
        .bind(offset)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

/// A page of the names that jobs use without a declaration, by name, with their jobs counted.
/// The names are walked by the indexes of `enqueued` and `claimed`, one probe per name, so the
/// cost follows the page and the declared flows it skips, not the number of waiting jobs. The
/// walk is ascending and `page` keeps its order (no `ORDER BY` inside: that would end the walk
/// only after every name).
pub async fn list_undeclared(
    pool: &Pool<Postgres>,
    after: Option<&str>,
    limit: i64,
    offset: i64,
) -> Result<Vec<UndeclaredFlowRow>, Error> {
    const SQL: &str = "
    WITH RECURSIVE names(flow_key) AS (
        SELECT (SELECT min(m) FROM (
                   SELECT min(flow_key) AS m FROM enqueued WHERE flow_key > coalesce($1::text, '')
                   UNION ALL
                   SELECT min(flow_key) FROM claimed WHERE flow_key > coalesce($1::text, '')) s)
        UNION ALL
        SELECT (SELECT min(m) FROM (
                   SELECT min(flow_key) AS m FROM enqueued WHERE flow_key > names.flow_key
                   UNION ALL
                   SELECT min(flow_key) FROM claimed WHERE flow_key > names.flow_key) s)
        FROM names
        WHERE names.flow_key IS NOT NULL
    ), page AS (
        SELECT flow_key FROM names
        WHERE flow_key IS NOT NULL
          AND NOT EXISTS (SELECT 1 FROM flows f WHERE f.flow_id = names.flow_key)
        LIMIT $2 OFFSET $3
    )
    SELECT p.flow_key AS flow_id,
           (SELECT count(*) FROM (SELECT 1 FROM enqueued e WHERE e.flow_key = p.flow_key LIMIT 101) w) AS waiting,
           (SELECT count(*) FROM claimed c WHERE c.flow_key = p.flow_key) AS processing
    FROM page p
    ORDER BY p.flow_key";
    let rows = sqlx::query_as::<_, UndeclaredFlowRow>(SQL)
        .bind(after)
        .bind(limit)
        .bind(offset)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

/// A declared flow.
pub async fn get_by_id(pool: &Pool<Postgres>, flow_id: &str) -> Result<Option<FlowRow>, Error> {
    const SQL: &str = "
    SELECT flow_id, concurrency, rate, rate_period, paused, next_at
    FROM flows WHERE flow_id = $1";
    let row = sqlx::query_as::<_, FlowRow>(SQL)
        .bind(flow_id)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

/// Declares a flow or replaces the limits of a declared one (the pause stays; the rate limit
/// starts afresh, a new rate takes effect at once). Gives the flow and whether it is new.
pub async fn upsert(pool: &Pool<Postgres>, flow: &FlowCreate) -> Result<(FlowRow, bool), Error> {
    #[derive(sqlx::FromRow)]
    struct Upserted {
        #[sqlx(flatten)]
        row: FlowRow,
        created: bool,
    }
    const SQL: &str = "
    INSERT INTO flows(flow_id, concurrency, rate, rate_period)
    VALUES ($1, $2, $3, $4)
    ON CONFLICT (flow_id) DO UPDATE
        SET concurrency = EXCLUDED.concurrency,
            rate = EXCLUDED.rate, rate_period = EXCLUDED.rate_period, next_at = NULL
    RETURNING flow_id, concurrency, rate, rate_period, paused, next_at,
              (xmax = 0) AS created";
    let done = sqlx::query_as::<_, Upserted>(SQL)
        .bind(&flow.flow_key)
        .bind(flow.concurrency)
        .bind(flow.rate)
        .bind(flow.rate_period)
        .fetch_one(pool)
        .await?;
    Ok((done.row, done.created))
}

/// Pauses a declared flow: its jobs wait, none is lost. `false` when it is not declared.
pub async fn pause(pool: &Pool<Postgres>, flow_id: &str) -> Result<bool, Error> {
    set_paused(pool, flow_id, true).await
}

/// Resumes a declared flow. `false` when it is not declared.
pub async fn resume(pool: &Pool<Postgres>, flow_id: &str) -> Result<bool, Error> {
    set_paused(pool, flow_id, false).await
}

async fn set_paused(pool: &Pool<Postgres>, flow_id: &str, paused: bool) -> Result<bool, Error> {
    let done = sqlx::query("UPDATE flows SET paused = $2 WHERE flow_id = $1")
        .bind(flow_id)
        .bind(paused)
        .execute(pool)
        .await?;
    Ok(done.rows_affected() > 0)
}

/// Removes the declaration: the jobs stay and the name goes back to one job at a time.
pub async fn delete(pool: &Pool<Postgres>, flow_id: &str) -> Result<bool, Error> {
    let done = sqlx::query("DELETE FROM flows WHERE flow_id = $1")
        .bind(flow_id)
        .execute(pool)
        .await?;
    Ok(done.rows_affected() > 0)
}
