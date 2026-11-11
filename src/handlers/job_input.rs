use super::http::{KEY_RULE, is_key};
use crate::features::schedules::JobSchedule;
use crate::models::headers::is_reserved_header;
use crate::models::{
    JobCreate, JobMeta, PRIORITY_RULE, ParamError, QUEUE_RULE, WorkerOptions, is_queue,
};
use axum::body::Bytes;
use base64::Engine as _;
use chrono::{DateTime, Utc};
use hyper::header::{HeaderName, HeaderValue};
use serde::Deserialize;
use serde_json::{Map, Value, value::RawValue};
use std::collections::HashMap;

/// JSON job format v1, flat like `enqueue_v1`, the `/to/` query and the "New job" form:
/// `sink` (`http` with `method`, `url`; `noop`), `retry` (`fixed`, `fibonacci`, `exponential` with
/// `retry_count`, `retry_delay`, which `fixed` may leave out to retry at once, and
/// `retry_max_delay` of the last two), `delay` (seconds), `timeout` (milliseconds), `queue`,
/// `priority`, `headers`, `body` or `body_base64`, `key`, `at`.
/// The retry policy defaults to none, the timeout to `--timeout` when the job runs. Fields that
/// are not known are ignored.
#[derive(Debug, Deserialize)]
pub(crate) struct JobInput {
    /// The fields of [`JobMeta`]: everything that is not one of the fields below.
    #[serde(flatten)]
    pub(crate) meta: Map<String, Value>,
    /// Headers for the target. Names that differ only by case are rejected.
    pub(crate) headers: Option<HashMap<String, String>>,
    /// A string is sent as it is, any other JSON value as JSON text (`content-type` defaults to JSON).
    pub(crate) body: Option<Box<RawValue>>,
    /// Base64 of a binary body, not together with `body`.
    pub(crate) body_base64: Option<String>,
    /// When the job runs: RFC 3339 or unix seconds. Wins over `delay`.
    pub(crate) at: Option<Value>,
    /// Seconds from now, when `at` is not set.
    pub(crate) delay: Option<u32>,
    /// A repeated key returns the existing job.
    pub(crate) key: Option<String>,
}

fn parse_at(at: &Value) -> Option<DateTime<Utc>> {
    match at {
        Value::Number(n) => n
            .as_i64()
            .and_then(|secs| DateTime::from_timestamp(secs, 0)),
        Value::String(s) => JobSchedule::parse_time(s),
        Value::Null | Value::Bool(_) | Value::Array(_) | Value::Object(_) => None,
    }
}

/// Validates the JSON description of a job: every invalid field is reported, not only the first.
pub(crate) fn prepare_input(
    options: &WorkerOptions,
    spec: JobInput,
    now: DateTime<Utc>,
) -> Result<JobCreate, Vec<ParamError>> {
    let mut errors = Vec::new();
    let mut bad = |parameter, detail: &str| errors.push(ParamError::new(parameter, detail));

    let mut meta = serde_json::from_value::<JobMeta>(Value::Object(spec.meta))
        .map_err(|err| bad("job", &err.to_string()))
        .ok();
    if let Some(meta) = &mut meta {
        // an empty queue is none, as in `enqueue_v1`
        meta.queue = meta.queue.take().filter(|queue| !queue.is_empty());
        match &meta.queue {
            Some(queue) if !is_queue(queue) => bad("queue", QUEUE_RULE),
            Some(_) if meta.priority.is_some_and(|p| p != 0) => bad("priority", PRIORITY_RULE),
            _ => {}
        }
        // none: the server's default when the job runs
        if meta.timeout == Some(0) {
            bad("timeout", "must be a positive number of milliseconds");
        }
        if let Err(detail) = meta.sink.check(options) {
            bad("sink", detail);
        }
    }

    let mut headers = normalize_headers(spec.headers.unwrap_or_default())
        .map_err(|detail| bad("headers", detail))
        .unwrap_or_default();
    let body = match (spec.body, spec.body_base64) {
        (Some(_), Some(_)) => {
            bad("body_base64", "cannot be set together with body");
            Bytes::new()
        }
        (None, Some(encoded)) => base64::engine::general_purpose::STANDARD
            .decode(encoded.trim())
            .map(Bytes::from)
            .map_err(|_| bad("body_base64", "is not valid base64"))
            .unwrap_or_default(),
        (None, None) => Bytes::new(),
        (Some(json), None) if json.get().starts_with('"') => {
            serde_json::from_str::<String>(json.get())
                .map(Bytes::from)
                .map_err(|err| bad("body", &err.to_string()))
                .unwrap_or_default()
        }
        (Some(json), None) => {
            headers
                .entry("content-type".into())
                .or_insert_with(|| "application/json".into());
            Bytes::from(json.get().to_owned())
        }
    };
    if spec.key.as_deref().is_some_and(|key| !is_key(key)) {
        bad("key", KEY_RULE);
    }
    let at = spec.at.as_ref().map(|at| {
        parse_at(at).unwrap_or_else(|| {
            bad("at", "must be an RFC 3339 time or unix seconds");
            now
        })
    });

    let Some(meta) = meta.filter(|_| errors.is_empty()) else {
        return Err(errors);
    };
    // `at` wins over `delay`
    let at = at.or_else(|| {
        spec.delay
            .map(|secs| now + chrono::Duration::seconds(i64::from(secs)))
    });
    Ok(JobCreate {
        meta,
        headers: Some(headers),
        body,
        at,
        schedule: None,
        key: spec.key,
    }
    .normalized(options, now))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::JobSink;

    #[test]
    fn json_job_preparation() -> anyhow::Result<()> {
        let options = WorkerOptions::for_tests();
        let now = Utc::now();
        let prepare = |spec| prepare_input(&options, spec, now);

        let spec = serde_json::from_str(
            r#"{"sink":"http","method":"POST","url":"https://example.com","delay":2,
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
            r#"{"sink":"http","method":"POST","url":"ftp://example.com","timeout":0,
                "headers":{"Authorization":"a","authorization":"b"},"key":""}"#,
        )?;
        let errors = prepare(spec).expect_err("invalid job");
        assert_eq!(
            errors
                .iter()
                .map(|error| error.parameter)
                .collect::<Vec<_>>(),
            ["timeout", "sink", "headers", "key"]
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
            let spec = serde_json::from_str(meta)?;
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
            let spec = serde_json::from_str(meta)?;
            let errors = prepare(spec).expect_err(meta);
            assert!(
                errors
                    .iter()
                    .all(|e| ["job", "sink"].contains(&e.parameter)),
                "{meta}"
            );
        }
        Ok(())
    }
}
