use crate::features;
use crate::features::callbacks::JobOutcome;
use crate::features::results::{self, JobResult};
use crate::models::{CLAIM_LEASE, Error, JobCreateRow, JobEntry, JobMeta, JobWithAttempt};
use crate::models::{JobCreate, JobRow};
use chrono::{DateTime, Utc};
use sqlx::{FromRow, Pool, Postgres, Row, types::Json};

/// Creates a job or declares a schedule, returning the row and whether the resource is new.
pub async fn create(
    pool: &Pool<Postgres>,
    mut job: JobCreate,
    instance_id: &str,
) -> Result<(JobCreateRow, bool), Error> {
    if job.schedule.is_some() {
        // as `PUT /schedules/{id}`: the key is the schedule id, a new ULID when absent
        job.key.get_or_insert_with(crate::models::new_ulid);
        return features::schedules::db::upsert(pool, job, instance_id).await;
    }
    let key = job.key.clone();
    let res = if let Some(at) = job.at {
        create_at(pool, job, at, instance_id).await
    } else {
        create_enqueue(pool, job, instance_id).await
    };
    // Check existing Job for idempotency
    if let Err(Error::DbError(sqlx::Error::Database(ref err))) = res
        && err.code().unwrap_or_default() == "23505"
        && let Some(key) = key
        && let Some(job) = get_id_by_key(pool, &key).await?
    {
        return Ok((job, false));
    }
    res.map(|job| (job, true))
}

async fn create_enqueue(
    pool: &Pool<Postgres>,
    job: JobCreate,
    instance_id: &str,
) -> Result<JobCreateRow, Error> {
    const SQL: &str = "
    WITH a AS (
        INSERT INTO jobs(meta, headers, body, key) VALUES ($1, $2, $3, $4) RETURNING id
    ), hist AS (
        INSERT INTO history(id, attempt, kind, instance_id) SELECT id, 0, 'enqueued', $5 FROM a
    )
    INSERT INTO enqueued(id, flow_key, priority) SELECT id, $6, $7 FROM a RETURNING id";

    let body = job.body_bytes();
    let (flow_key, priority) = (
        job.meta
            .flow_key
            .as_ref()
            .map(crate::models::FlowKey::as_str),
        job.meta.priority.unwrap_or(0),
    );
    let job_id = sqlx::query_scalar::<_, i64>(SQL)
        .bind(Json(&job.meta))
        .bind(Json(&job.headers))
        .bind(body)
        .bind(&job.key)
        .bind(instance_id)
        .bind(flow_key)
        .bind(priority)
        .fetch_one(pool)
        .await?;
    Ok(JobCreateRow {
        id: job_id,
        schedule_id: None,
        key: job.key,
    })
}

async fn create_at(
    pool: &Pool<Postgres>,
    job: JobCreate,
    at: DateTime<Utc>,
    instance_id: &str,
) -> Result<JobCreateRow, Error> {
    const SQL: &str = "
    WITH a AS (
        INSERT INTO jobs(meta, headers, body, key) VALUES ($1, $2, $3, $4) RETURNING id
    ), hist AS (
        INSERT INTO history(id, attempt, kind, instance_id) SELECT id, 0, 'scheduled', $6 FROM a
    )
    INSERT INTO scheduled(id, at, flow_key, priority) SELECT id, $5::timestamptz, $7, $8 FROM a RETURNING id
    ";

    let body = job.body_bytes();
    let (flow_key, priority) = (
        job.meta
            .flow_key
            .as_ref()
            .map(crate::models::FlowKey::as_str),
        job.meta.priority.unwrap_or(0),
    );

    let job_id = sqlx::query_scalar::<_, i64>(SQL)
        .bind(Json(&job.meta))
        .bind(Json(&job.headers))
        .bind(body)
        .bind(&job.key)
        .bind(at)
        .bind(instance_id)
        .bind(flow_key)
        .bind(priority)
        .fetch_one(pool)
        .await?;
    Ok(JobCreateRow {
        id: job_id,
        schedule_id: None,
        key: job.key,
    })
}

pub async fn get_by_id(pool: &Pool<Postgres>, job_id: i64) -> Result<Option<JobRow>, Error> {
    const SQL: &str = "SELECT * FROM jobs WHERE id = $1";
    let job = sqlx::query_as::<_, JobRow>(SQL)
        .bind(job_id)
        .fetch_optional(pool)
        .await?;
    Ok(job)
}

/// The jobs made by a job (its callbacks), oldest first: a page of `limit` jobs after the job
/// `after` (the last one of the previous page).
pub async fn list_by_parent(
    pool: &Pool<Postgres>,
    parent_id: i64,
    after: Option<i64>,
    limit: i64,
) -> Result<Vec<JobRow>, Error> {
    const SQL: &str = "
    SELECT * FROM jobs
    WHERE parent_id = $1 AND ($2::bigint IS NULL OR id > $2)
    ORDER BY id LIMIT $3";
    let jobs = sqlx::query_as::<_, JobRow>(SQL)
        .bind(parent_id)
        .bind(after)
        .bind(limit)
        .fetch_all(pool)
        .await?;
    Ok(jobs)
}

pub async fn get_id_by_key(
    pool: &Pool<Postgres>,
    key: &str,
) -> Result<Option<JobCreateRow>, Error> {
    const SQL: &str = "SELECT id, schedule_id, key FROM jobs WHERE key = $1";
    let job = sqlx::query_as::<_, JobCreateRow>(SQL)
        .bind(key)
        .fetch_optional(pool)
        .await?;
    Ok(job)
}

pub async fn delete(pool: &Pool<Postgres>, job_id: i64) -> Result<u64, Error> {
    const SQL: &str = "DELETE FROM jobs WHERE id = $1";
    let res = sqlx::query(SQL).bind(job_id).execute(pool).await?;
    Ok(res.rows_affected())
}

/// Moves up to `limit` due jobs from `scheduled` to `enqueued`.
pub async fn enqueue_scheduled(
    pool: &Pool<Postgres>,
    instance_id: &str,
    limit: i32,
) -> Result<u64, Error> {
    const SQL: &str = "
    WITH a AS (
        DELETE FROM scheduled
        WHERE id = ANY(ARRAY(SELECT id FROM scheduled WHERE at <= now() ORDER BY at, id LIMIT $1 FOR UPDATE SKIP LOCKED))
        RETURNING id, attempt, at, flow_key, priority
    ), b AS (
        INSERT INTO enqueued(id, attempt, at, flow_key, priority) SELECT id, attempt, at, flow_key, priority FROM a RETURNING id, attempt
    )
    INSERT INTO history(id, attempt, kind, instance_id) SELECT id, attempt, 'enqueued', $2 FROM b";
    let res = sqlx::query(SQL)
        .bind(limit)
        .bind(instance_id)
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}

/// The retries of [`retry_many`] as columns, one entry per job.
#[derive(Debug, Default)]
pub struct Retries {
    ids: Vec<i64>,
    attempts: Vec<i32>,
    /// `None`: at once, to `enqueued`.
    ats: Vec<Option<DateTime<Utc>>>,
    messages: Vec<String>,
}

impl Retries {
    pub fn push(&mut self, entry: JobEntry, at: Option<DateTime<Utc>>, message: String) {
        self.ids.push(entry.id);
        self.attempts.push(entry.attempt);
        self.ats.push(at);
        self.messages.push(message);
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.ids.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    pub fn entries(&self) -> impl Iterator<Item = JobEntry> + '_ {
        let zipped = self.ids.iter().zip(&self.attempts);
        zipped.map(|(&id, &attempt)| JobEntry { id, attempt })
    }
}

/// Puts jobs back for another attempt (`attempt + 1`) in one statement: to `enqueued` at once
/// (`at` is `None`) or to `scheduled` at `at`. Only the current attempt (`instance_id` +
/// `attempt`) may do it: returns the number of jobs put back.
pub async fn retry_many(
    pool: &Pool<Postgres>,
    instance_id: &str,
    retries: &Retries,
) -> Result<u64, Error> {
    const SQL: &str = "
    WITH input AS (
        SELECT * FROM unnest($2::bigint[], $3::int[], $4::timestamptz[], $5::text[]) AS t(id, attempt, at, message)
    ), a AS (
        DELETE FROM claimed AS c
        WHERE c.id = ANY($2::bigint[]) AND c.instance_id = $1
          AND c.attempt = ($3::int[])[array_position($2::bigint[], c.id)]
        RETURNING c.id, c.attempt + 1 AS attempt, c.flow_key, c.priority
    ), e AS (
        INSERT INTO enqueued(id, attempt, flow_key, priority)
        SELECT a.id, a.attempt, a.flow_key, a.priority FROM a INNER JOIN input AS i ON i.id = a.id
        WHERE i.at IS NULL
    ), s AS (
        INSERT INTO scheduled(id, at, attempt, flow_key, priority)
        SELECT a.id, i.at, a.attempt, a.flow_key, a.priority FROM a INNER JOIN input AS i ON i.id = a.id
        WHERE i.at IS NOT NULL
    )
    INSERT INTO history(id, attempt, kind, instance_id, message)
    SELECT a.id, a.attempt, 'retried', $1, i.message FROM a INNER JOIN input AS i ON i.id = a.id";
    let res = sqlx::query(SQL)
        .bind(instance_id)
        .bind(&retries.ids)
        .bind(&retries.attempts)
        .bind(&retries.ats)
        .bind(&retries.messages)
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}

/// Renews by [`CLAIM_LEASE`] the leases of the jobs this instance still holds: `ids` and
/// `attempts` are the columns of [`crate::models::Held`].
pub async fn renew_leases(
    pool: &Pool<Postgres>,
    instance_id: &str,
    ids: &[i64],
    attempts: &[i32],
) -> Result<u64, Error> {
    const SQL: &str = "
    UPDATE claimed AS c SET until = now() + $4
    FROM unnest($2::bigint[], $3::int[]) AS h(id, attempt)
    WHERE c.id = h.id AND c.attempt = h.attempt AND c.instance_id = $1";
    let res = sqlx::query(SQL)
        .bind(instance_id)
        .bind(ids)
        .bind(attempts)
        .bind(CLAIM_LEASE)
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}

/// Releases up to `limit` jobs whose lease has ended (`attempt + 1`, so a late result of the
/// stuck attempt is rejected), whatever instance holds them.
pub async fn reap_expired_locks(
    pool: &Pool<Postgres>,
    instance_id: &str,
    limit: i32,
) -> Result<u64, Error> {
    const SQL: &str = "
    WITH a AS (
        DELETE FROM claimed
        WHERE id = ANY(ARRAY(SELECT id FROM claimed WHERE until < now() ORDER BY until LIMIT $2 FOR UPDATE SKIP LOCKED))
        RETURNING id, attempt, flow_key, priority, instance_id
    ), b AS (
        INSERT INTO enqueued(id, attempt, flow_key, priority) SELECT id, attempt + 1, flow_key, priority FROM a
    )
    INSERT INTO history(id, attempt, kind, instance_id, message)
    SELECT id, attempt + 1, 'retried', $1, 'lock expired: ' || instance_id FROM a";
    let res = sqlx::query(SQL)
        .bind(instance_id)
        .bind(limit)
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}

/// Claims up to `prefetch` enqueued jobs for `instance_id` and returns them (`claim_v1`: the
/// jobs without a flow first, the flows get what is left, in one call and one transaction).
///
/// `claim_v1` reads the jobs in the statement that claims them: a join with `jobs` outside
/// would use an older snapshot and miss a job enqueued meanwhile, which would stay claimed and
/// never run. The claim is committed before any job runs: a worker completing a job never
/// waits on the fetch itself. Each claim has a lease of [`CLAIM_LEASE`].
pub async fn fetch_job_with_attempt(
    pool: &Pool<Postgres>,
    instance_id: &str,
    prefetch: i32,
) -> Result<Vec<JobWithAttempt>, Error> {
    const SQL: &str = "SELECT * FROM claim_v1($2, $1, $3)";
    let rows = sqlx::query(SQL)
        .bind(prefetch)
        .bind(instance_id)
        .bind(CLAIM_LEASE)
        .fetch_all(pool)
        .await?;
    let mut jobs = Vec::with_capacity(rows.len());
    for row in rows {
        match JobWithAttempt::from_row(&row) {
            Ok(job) => jobs.push(job),
            // an unreadable job (a bad URL, say) fails alone, it must not hold the batch
            Err(err) => {
                let (Ok(id), Ok(attempt)) = (row.try_get("id"), row.try_get("attempt")) else {
                    tracing::error!("unreadable job without an id: {err:?}");
                    continue;
                };
                let entry = JobEntry { id, attempt };
                tracing::error!(job_id = id, "unreadable job: {err:?}");
                let result = JobResult::from(Error::from(err));
                // if this fails too, the job stays claimed (the reaper frees it); the batch goes on
                _ = results::processed(pool, entry, instance_id, &result, None)
                    .await
                    .inspect_err(|err| {
                        tracing::error!(job_id = id, "result of an unreadable job: {err:?}");
                    });
            }
        }
    }
    Ok(jobs)
}

/// What a user action did to a job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionOutcome {
    Done,
    /// The job is not in a state the action applies to (running, already processed...).
    Conflict,
    NotFound,
}

async fn outcome(
    pool: &Pool<Postgres>,
    affected: u64,
    job_id: i64,
) -> Result<ActionOutcome, Error> {
    if affected > 0 {
        return Ok(ActionOutcome::Done);
    }
    let exists: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM jobs WHERE id = $1)")
        .bind(job_id)
        .fetch_one(pool)
        .await?;
    Ok(if exists {
        ActionOutcome::Conflict
    } else {
        ActionOutcome::NotFound
    })
}

/// Cancels a pending job (scheduled, or enqueued and not running): it ends as `cancelled`.
/// A running or processed job is a conflict.
///
/// The callback uses the attempt actually removed from the queue and commits with the cancellation.
/// Only a user's cancel makes one: a run that its schedule takes back is not an outcome of the
/// job that anybody waits for. `taken_back` is `None` for a user's cancel, and for a schedule the
/// message of the history: the cancel makes no callback.
pub async fn cancel(
    conn: &mut sqlx::PgConnection,
    job_id: i64,
    instance_id: &str,
    taken_back: Option<&str>,
) -> Result<u64, Error> {
    const SQL_SOURCE: &str = "
    SELECT meta, key, schedule_id FROM jobs WHERE id = $1 AND meta ? 'callback'";
    const SQL_CANCEL: &str = "
    WITH s AS (
        DELETE FROM scheduled WHERE id = $1 RETURNING id, attempt
    ), e AS (
        DELETE FROM enqueued WHERE id = $1 RETURNING id, attempt
    ), c AS (
        SELECT id, attempt FROM s UNION ALL SELECT id, attempt FROM e
    ), hist AS (
        INSERT INTO history(id, attempt, kind, instance_id, message)
        SELECT id, attempt, 'cancelled', $2, $7 FROM c
    ), p AS (
        INSERT INTO processed(id, attempt, instance_id, status)
        SELECT id, attempt, $2, 'cancelled' FROM c
        RETURNING id, attempt
    ), cb AS (
        INSERT INTO jobs(meta, headers, body, key, parent_id)
        SELECT $3::jsonb, $4::jsonb,
            CASE WHEN $3::jsonb->>'callback_as' = 'webhook' THEN
                convert_to(jsonb_set(convert_from($5::bytea, 'UTF8')::jsonb,
                    '{data,job,attempt}', to_jsonb(p.attempt))::text, 'UTF8')
            ELSE $5::bytea END,
            $6::text, p.id
        FROM p WHERE $3::jsonb IS NOT NULL
        RETURNING id, parent_id
    ), cbe AS (
        INSERT INTO enqueued(id, flow_key, priority)
        SELECT cb.id, $3::jsonb->>'flow_key', coalesce(($3::jsonb->>'priority')::int, 0) FROM cb
    ), cbh AS (
        INSERT INTO history(id, attempt, kind, message)
        SELECT cb.id, 0, 'enqueued', 'callback of #' || cb.parent_id FROM cb
    )
    SELECT EXISTS (SELECT 1 FROM p)";
    #[derive(sqlx::FromRow)]
    struct Source {
        meta: Json<serde_json::Value>,
        key: Option<String>,
        schedule_id: Option<String>,
    }
    let source = match taken_back {
        None => {
            sqlx::query_as::<_, Source>(SQL_SOURCE)
                .bind(job_id)
                .fetch_optional(&mut *conn)
                .await?
        }
        Some(_) => None,
    };
    let spawned = source.and_then(|source| {
        // an unreadable meta (by hand, another version): no callback, the job is still cancelled
        let meta: JobMeta = serde_json::from_value(source.meta.0)
            .inspect_err(|err| tracing::warn!(job_id, "cancel: no callback, the meta: {err}"))
            .ok()?;
        // SQL puts the removed attempt into the webhook; a cancel has no trace of an attempt
        JobOutcome {
            id: job_id,
            key: source.key.as_deref(),
            schedule_id: source.schedule_id.as_deref(),
            meta: &meta,
            attempt: 0,
            result: &JobResult::cancelled(),
            trace: None,
        }
        .callback()
    });
    // once more with a new snapshot: a promotion in flight moved the job to `enqueued`
    for _ in 0..2 {
        let cancelled = sqlx::query_scalar::<_, bool>(SQL_CANCEL)
            .bind(job_id)
            .bind(instance_id)
            .bind(spawned.as_ref().map(|job| Json(&job.meta)))
            .bind(spawned.as_ref().map(|job| Json(&job.headers)))
            .bind(spawned.as_ref().and_then(JobCreate::body_bytes))
            .bind(spawned.as_ref().and_then(|job| job.key.as_deref()))
            .bind(taken_back.unwrap_or("cancelled by a user"))
            .fetch_one(&mut *conn)
            .await?;
        if cancelled {
            return Ok(1);
        }
    }
    Ok(0)
}

pub async fn cancel_job(
    pool: &Pool<Postgres>,
    job_id: i64,
    instance_id: &str,
) -> Result<ActionOutcome, Error> {
    let affected = {
        let mut conn = pool.acquire().await?;
        cancel(&mut conn, job_id, instance_id, None).await?
    };
    outcome(pool, affected, job_id).await
}

/// Runs a processed job (completed, failed or cancelled) once more, now.
/// The attempt number goes on (it fences the attempts), so the retry policy is not restarted.
pub async fn requeue_job(
    pool: &Pool<Postgres>,
    job_id: i64,
    instance_id: &str,
) -> Result<ActionOutcome, Error> {
    const SQL: &str = "
    WITH p AS (
        DELETE FROM processed WHERE id = $1 RETURNING id, attempt
    ), e AS (
        INSERT INTO enqueued(id, attempt, flow_key, priority)
        SELECT p.id, p.attempt + 1, j.meta->>'flow_key', coalesce((j.meta->>'priority')::int, 0)
        FROM p INNER JOIN jobs AS j ON j.id = p.id
        RETURNING id, attempt
    )
    INSERT INTO history(id, attempt, kind, instance_id, message)
    SELECT id, attempt, 'enqueued', $2, 'retried by a user' FROM e";
    let res = sqlx::query(SQL)
        .bind(job_id)
        .bind(instance_id)
        .execute(pool)
        .await?;
    outcome(pool, res.rows_affected(), job_id).await
}
