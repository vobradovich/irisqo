use super::http::{KEY_RULE, create_job, is_key, job_headers, parse_body};
use crate::models::headers::is_reserved_header;
use crate::models::{Error, JobCreate, JobMeta, ParamError, Problem, WorkerOptions};
use crate::otel;
use crate::{db, db::jobqueue::ActionOutcome, models::AppState};
use axum::{
    Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use chrono::{DateTime, Utc};
use hyper::header::{HeaderName, HeaderValue};
use serde::Deserialize;
use serde_json::{Map, Value, value::RawValue};
use std::{collections::HashMap, sync::Arc};

pub fn routes(state: Arc<AppState>) -> Router {
    Router::new()
        .route(
            "/jobs",
            post(create).layer(DefaultBodyLimit::max(state.max_body_size)),
        )
        .route("/jobs/{id}", get(get_by_id).delete(delete_by_id))
        .route("/jobs/{id}/cancel", post(cancel))
        .route("/jobs/{id}/retry", post(retry))
        .with_state(state)
}

/// JSON job format v1. `meta` is the stored [`JobMeta`]: `sink` (`http` with `method`, `url`),
/// `retry` (`fixed`, `fibonacci`, `exponential` with `retry_count`, `retry_delay`, which `fixed` may
/// leave out to retry at once, and `retry_max_delay` of the last two), `delay`, `timeout`.
/// The retry policy defaults to none, the timeout to `--timeout`; an HTTP job needs `method` and `url`.
/// Unknown fields of `meta` are ignored.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct JobSpec {
    pub(crate) meta: Map<String, Value>,
    /// Headers for the target. Names that differ only by case are rejected.
    pub(crate) headers: Option<HashMap<String, String>>,
    /// A string is sent as it is, any other JSON value as JSON text (`content-type` defaults to JSON).
    pub(crate) body: Option<Box<RawValue>>,
    /// A repeated key returns the existing job.
    pub(crate) key: Option<String>,
}

/// Validates the JSON description of a job: every invalid field is reported, not only the first.
pub(crate) fn prepare_spec(
    options: &WorkerOptions,
    spec: JobSpec,
    now: DateTime<Utc>,
) -> Result<JobCreate, Vec<ParamError>> {
    let mut errors = Vec::new();
    let mut bad = |parameter, detail: &str| errors.push(ParamError::new(parameter, detail));

    let mut meta = serde_json::from_value::<JobMeta>(Value::Object(spec.meta))
        .map_err(|err| bad("meta", &err.to_string()))
        .ok();
    if let Some(meta) = &mut meta {
        // none: the server's default when the job runs
        if meta.timeout == Some(0) {
            bad("meta.timeout", "must be a positive number of milliseconds");
        }
        meta.timeout = meta.timeout.map(|ms| ms.min(options.max_timeout));
        if let Err(detail) = meta.sink.check(options) {
            bad("meta.sink", detail);
        }
        // the trace context of this request, unless the job brings its own
        if meta.traceparent.is_none() {
            (meta.traceparent, meta.tracestate) = otel::current_trace_headers();
        }
    }

    let mut headers = normalize_headers(spec.headers.unwrap_or_default())
        .map_err(|detail| bad("headers", detail))
        .unwrap_or_default();
    let body = match spec.body {
        None => Bytes::new(),
        Some(json) if json.get().starts_with('"') => serde_json::from_str::<String>(json.get())
            .map(Bytes::from)
            .map_err(|err| bad("body", &err.to_string()))
            .unwrap_or_default(),
        Some(json) => {
            headers
                .entry("content-type".into())
                .or_insert_with(|| "application/json".into());
            Bytes::from(json.get().to_owned())
        }
    };
    if spec.key.as_deref().is_some_and(|key| !is_key(key)) {
        bad("key", KEY_RULE);
    }

    let Some(meta) = meta.filter(|_| errors.is_empty()) else {
        return Err(errors);
    };
    // `delay` stays in the meta, the job waits in `scheduled` until `at`
    let at = meta
        .delay
        .map(|secs| now + chrono::Duration::seconds(i64::from(secs)));
    Ok(JobCreate {
        meta,
        headers: Some(headers),
        body,
        at,
        schedule: None,
        key: spec.key,
    })
}

fn normalize_headers(
    headers: HashMap<String, String>,
) -> Result<HashMap<String, String>, &'static str> {
    let mut normalized = HashMap::with_capacity(headers.len());
    for (name, value) in headers {
        let name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| "has an invalid name")?;
        if is_reserved_header(name.as_str()) {
            return Err("contains a reserved header name");
        }
        HeaderValue::from_str(&value).map_err(|_| "has an invalid value")?;
        if normalized.insert(name.as_str().to_owned(), value).is_some() {
            return Err("contains duplicate header names");
        }
    }
    Ok(normalized)
}

/// Creates a job from JSON: `200` with the job (`id`, `key`) and the same in the headers
/// (`Location`, `Irisqo-*`). A repeated `key` returns the existing job.
async fn create(
    State(state): State<Arc<AppState>>,
    body: Bytes,
) -> Result<impl IntoResponse, Problem> {
    let spec: JobSpec = parse_body(&body, "job")?;
    let job =
        prepare_spec(&state.worker_options, spec, Utc::now()).map_err(Error::InvalidParams)?;
    let job = create_job(&state, job).await?;
    Ok((job_headers(&job), Json(job)))
}

async fn delete_by_id(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> Result<StatusCode, Problem> {
    let _ = db::jobqueue::delete(&state.pool, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn get_by_id(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> Result<Response, Problem> {
    let job = db::jobqueue::get_by_id(&state.pool, id)
        .await?
        .ok_or(Error::JobNotFound(id))?;
    Ok(Json(job).into_response())
}

fn action_status(outcome: ActionOutcome) -> StatusCode {
    match outcome {
        ActionOutcome::Done => StatusCode::NO_CONTENT,
        ActionOutcome::Conflict => StatusCode::CONFLICT,
        ActionOutcome::NotFound => StatusCode::NOT_FOUND,
    }
}

/// Cancels a pending job; 409 when it is running or already processed.
async fn cancel(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> Result<StatusCode, Problem> {
    let outcome = db::jobqueue::cancel_job(&state.pool, id, &state.instance_id).await?;
    Ok(action_status(outcome))
}

/// Runs a processed job once more; 409 when it is still pending or running.
async fn retry(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> Result<StatusCode, Problem> {
    let outcome = db::jobqueue::requeue_job(&state.pool, id, &state.instance_id).await?;
    Ok(action_status(outcome))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::JobSink;

    #[test]
    fn json_job_preparation() -> anyhow::Result<()> {
        let options = WorkerOptions::for_tests();
        let now = Utc::now();
        let prepare = |spec| prepare_spec(&options, spec, now);

        let spec = serde_json::from_str(
            r#"{"meta":{"sink":"http","method":"POST","url":"https://example.com","delay":2},
                "headers":{"X-Test":"hello"},"body":{"n":18446744073709551617}}"#,
        )?;
        let job = prepare(spec).expect("valid job");
        assert!(
            matches!(&job.meta.sink, JobSink::Http(http) if http.method == hyper::Method::POST)
        );
        assert_eq!(job.meta.retry, None);
        // none stored: the server's default applies when the job runs
        assert_eq!(job.meta.timeout, None);
        assert_eq!(job.at, Some(now + chrono::Duration::seconds(2)));
        assert_eq!(job.body.as_ref(), br#"{"n":18446744073709551617}"#);
        assert_eq!(
            job.headers,
            Some(HashMap::from([
                ("x-test".into(), "hello".into()),
                ("content-type".into(), "application/json".into()),
            ]))
        );

        // every invalid field is reported
        let spec = serde_json::from_str(
            r#"{"meta":{"sink":"http","method":"POST","url":"ftp://example.com","timeout":0},
                "headers":{"Authorization":"a","authorization":"b"},"key":""}"#,
        )?;
        let errors = prepare(spec).expect_err("invalid job");
        assert_eq!(
            errors
                .iter()
                .map(|error| error.parameter)
                .collect::<Vec<_>>(),
            ["meta.timeout", "meta.sink", "headers", "key"]
        );
        assert!(
            errors
                .iter()
                .any(|error| error.detail.contains("duplicate"))
        );

        // a retry that is not a policy means no retries, not an error (see `JobMeta::retry`)
        for meta in [
            r#"{"sink":"noop","retry":"fixd"}"#,
            r#"{"sink":"noop","retry":"fibonacci","retry_count":3}"#,
            r#"{"sink":"noop","retry":"none"}"#,
        ] {
            let spec = serde_json::from_str(&format!(r#"{{"meta":{meta}}}"#))?;
            assert_eq!(prepare(spec).expect(meta).meta.retry, None, "{meta}");
        }

        // the structure of meta: no sink, an unknown sink, no url, a bad method
        for meta in [
            r#"{"url":"https://example.com"}"#,
            r#"{"sink":"smtp"}"#,
            r#"{"sink":"http"}"#,
            r#"{"sink":"http","url":"https://example.com"}"#,
            r#"{"sink":"http","url":"https://example.com","method":"BAD METHOD"}"#,
        ] {
            let spec = serde_json::from_str(&format!(r#"{{"meta":{meta}}}"#))?;
            let errors = prepare(spec).expect_err(meta);
            assert!(
                errors.iter().all(|e| e.parameter.starts_with("meta")),
                "{meta}"
            );
        }
        Ok(())
    }
}
