use crate::{
    db,
    features::schedules::JobSchedule,
    models::{
        AppState, Error, HttpMeta, JobCreate, JobCreateRow, JobMeta, JobProtocol, JobRetry,
        ParamError, WorkerOptions,
    },
    otel::{self, metrics::metrics},
};
use axum::{
    Router,
    body::Bytes,
    extract::{Path, RawQuery, State},
    http::{HeaderMap, HeaderValue, StatusCode},
    response::IntoResponse,
    routing::get,
};
use hyper::{Method, Uri};
use opentelemetry::KeyValue;
use problemdetails::Problem;

use std::{collections::HashMap, sync::Arc};
#[allow(unused_imports)]
use tracing::{debug, error, info, warn};
use url::{Url, form_urlencoded};

pub fn routes(state: Arc<AppState>) -> Router {
    Router::new()
        .route(
            "/to/{*url}",
            get(job_create)
                .post(job_create)
                .put(job_create)
                .delete(job_create)
                .patch(job_create),
        )
        .with_state(state)
}

async fn job_create(
    State(state): State<Arc<AppState>>,
    method: Method,
    Path(url): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: Bytes,
) -> Result<impl IntoResponse, Problem> {
    let request = JobRequest {
        method,
        url: &url,
        query: query.as_deref(),
        headers: forwarded_headers(&headers),
        body,
    };
    let job = prepare_job(&state.worker_options, request, JobSchedule::now_secs())
        .map_err(Error::Validation)?;
    let job = create_job(&state, job).await?;
    let mut headers = HeaderMap::new();
    // schedule-id and external-id (user-provided) may not be valid header values
    let mut put = |name: &'static str, value: Option<&str>| {
        if let Some(value) = value
            && let Ok(value) = HeaderValue::from_str(value)
        {
            headers.insert(name, value);
        }
    };
    put("location", Some(&format!("/api/v1/jobs/{}", job.id)));
    put("job-id", Some(&job.id.to_string()));
    put("schedule-id", job.schedule_id.as_deref());
    put("external-id", job.external_id.as_deref());
    Ok((StatusCode::CREATED, headers))
}

/// A job as requested through `/to/{url}?{query}`; the UI form builds the same.
#[derive(Debug)]
pub struct JobRequest<'a> {
    pub method: Method,
    /// The target URL, without the query string.
    pub url: &'a str,
    /// The `_*` job parameters mixed with the target's own query parameters.
    pub query: Option<&'a str>,
    /// The headers for the target (see [`forwarded_headers`]).
    pub headers: HashMap<String, String>,
    pub body: Bytes,
}

/// Validates a job request and builds the job: every invalid parameter is reported,
/// not only the first one. `_*` parameters are the job's, the others stay on the URL.
pub fn prepare_job(
    options: &WorkerOptions,
    request: JobRequest<'_>,
    now_secs: i64,
) -> Result<JobCreate, Vec<ParamError>> {
    let mut errors = Vec::new();
    let mut delay: Option<u32> = None;
    let mut at: Option<i64> = None;
    let mut timeout: u32 = options.timeout;
    let mut retry = JobRetry::None;
    let mut schedule: Option<JobSchedule> = None;
    let mut until: Option<i64> = None;
    let mut external_id: Option<String> = None;

    let mut url = Url::parse(request.url)
        .inspect_err(|_| errors.push(ParamError::new("url", "is not a valid URL")))
        .ok();
    let pairs = request
        .query
        .map(|qs| form_urlencoded::parse(qs.as_bytes()).collect::<Vec<_>>())
        .unwrap_or_default();
    for (key, value) in pairs {
        match key.as_ref() {
            "_delay" => match value.parse::<u32>() {
                Ok(secs) => {
                    delay = Some(secs);
                    at = Some(now_secs + i64::from(secs));
                }
                Err(_) => errors.push(ParamError::new("_delay", "must be a number of seconds")),
            },
            "_delay_until" => match value.parse::<i64>() {
                Ok(t) if JobSchedule::is_valid_unix_secs(t) => {
                    // a time in the past runs the job now
                    at = (t > now_secs).then_some(t);
                    delay = at.and_then(|t| u32::try_from(t - now_secs).ok());
                }
                _ => errors.push(ParamError::new(
                    "_delay_until",
                    "must be a unix time in seconds",
                )),
            },
            "_timeout" => match value.parse::<u32>() {
                Ok(ms) if ms > 0 => timeout = ms.min(options.max_timeout),
                _ => errors.push(ParamError::new(
                    "_timeout",
                    "must be a positive number of milliseconds",
                )),
            },
            "_retry" => match value.parse::<JobRetry>() {
                Ok(parsed) => retry = parsed,
                Err(_) => errors.push(ParamError::new(
                    "_retry",
                    "must be N, N|fixed|S or N|fibonacci|S",
                )),
            },
            "_interval" => match value.parse::<u32>() {
                Ok(secs) if secs > 0 => schedule = Some(JobSchedule::Interval { interval: secs }),
                _ => errors.push(ParamError::new(
                    "_interval",
                    "must be a positive number of seconds",
                )),
            },
            "_cron" => match parse_cron(&value) {
                Ok(parsed) => schedule = Some(parsed),
                Err(message) => errors.push(ParamError::new("_cron", message)),
            },
            "_until" => match value.parse::<i64>() {
                Ok(t) if JobSchedule::is_valid_unix_secs(t) => until = Some(t),
                _ => errors.push(ParamError::new("_until", "must be a unix time in seconds")),
            },
            "_id" if value.chars().count() > 64 => {
                errors.push(ParamError::new("_id", "must be at most 64 characters"));
            }
            "_id" => external_id = (!value.is_empty()).then(|| value.into_owned()),
            _ => {
                if let Some(url) = url.as_mut() {
                    if value.is_empty() {
                        url.query_pairs_mut().append_key_only(&key);
                    } else {
                        url.query_pairs_mut().append_pair(&key, &value);
                    }
                }
            }
        }
    }
    let uri = url.and_then(|url| {
        Uri::try_from(url.as_str())
            .inspect_err(|_| errors.push(ParamError::new("url", "is not a valid URL")))
            .ok()
    });
    let Some(uri) = uri else {
        return Err(errors);
    };
    if !errors.is_empty() {
        return Err(errors);
    }
    let protocol = match uri.scheme_str() {
        Some("http" | "https") => JobProtocol::Http(HttpMeta {
            method: request.method,
            url: uri,
        }),
        _ => JobProtocol::None,
    };
    // Trace context of this request: the parent of every attempt
    let (traceparent, tracestate) = otel::current_trace_headers();
    Ok(JobCreate {
        meta: JobMeta {
            protocol,
            retry,
            delay,
            timeout,
            traceparent,
            tracestate,
        },
        headers: Some(request.headers),
        body: request.body,
        at,
        schedule,
        until,
        external_id,
    })
}

/// A cron schedule (5 or 6 fields, seconds first) with a message for a human.
fn parse_cron(value: &str) -> Result<JobSchedule, String> {
    let fields = value.split_whitespace().count();
    if !(5..=7).contains(&fields) {
        return Err(format!(
            "needs 5 or 6 fields, got {fields}: e.g. */15 * * * *"
        ));
    }
    match value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .parse()
    {
        Ok(schedule @ JobSchedule::Cron { .. }) => Ok(schedule),
        Ok(JobSchedule::Interval { .. }) | Err(_) => Err("is not a valid cron expression".into()),
    }
}

/// Stores a prepared job (or returns the existing one with the same external id).
pub async fn create_job(state: &AppState, job: JobCreate) -> Result<JobCreateRow, Error> {
    debug!("{:?}", serde_json::to_string(&job.meta));
    let job_type = if job.schedule.is_some() {
        "scheduled"
    } else if job.at.is_some() {
        "delayed"
    } else {
        "immediate"
    };
    let created = db::jobqueue::create(&state.pool, job, &state.instance_id).await?;
    metrics()
        .jobs_created
        .add(1, &[KeyValue::new("job.type", job_type)]);
    Ok(created)
}

/// Prefix of the request headers forwarded to the target, without the prefix.
pub const FORWARD_PREFIX: &str = "irisqo-forward-";

/// The headers forwarded to the target: the body ones (`content-type`, `content-encoding`,
/// `content-language`) and the `irisqo-forward-*` ones without the prefix
/// (`irisqo-forward-authorization: Bearer x` -> `authorization: Bearer x`).
/// Everything else (the caller's own `authorization`, `cookie`, proxy headers...) stays here.
pub fn forwarded_headers(headers: &HeaderMap) -> HashMap<String, String> {
    headers
        .iter()
        .filter_map(|(name, value)| {
            let name = name.as_str();
            let target = name.strip_prefix(FORWARD_PREFIX).or_else(|| {
                matches!(
                    name,
                    "content-type" | "content-encoding" | "content-language"
                )
                .then_some(name)
            })?;
            let value = value.to_str().ok()?;
            (!is_reserved_header(target)).then(|| (target.to_owned(), value.to_owned()))
        })
        .collect()
}

/// Headers a client cannot set on the target request (lowercase name): the HTTP client
/// sets the host and the body length, irisqo the trace context and the `irisqo-*` ones.
pub fn is_reserved_header(name: &str) -> bool {
    matches!(
        name,
        "" | "host"
            | "content-length"
            | "transfer-encoding"
            | "connection"
            | "traceparent"
            | "tracestate"
    ) || name.starts_with("irisqo-")
}

#[test]
fn forwarded_headers_allowlist() {
    let mut headers = HeaderMap::new();
    for (name, value) in [
        ("content-type", "application/json"),
        ("authorization", "Bearer caller"),
        ("cookie", "session=caller"),
        ("x-custom", "dropped"),
        ("irisqo-forward-authorization", "Bearer target"),
        ("irisqo-forward-x-custom", "kept"),
        ("irisqo-forward-host", "evil.example"),
        ("irisqo-forward-irisqo-job-id", "spoofed"),
        ("irisqo-forward-", "empty"),
        (
            "traceparent",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
        ),
    ] {
        headers.insert(
            axum::http::HeaderName::from_static(name),
            HeaderValue::from_static(value),
        );
    }
    let forwarded = forwarded_headers(&headers);
    assert_eq!(
        forwarded,
        HashMap::from([
            ("content-type".into(), "application/json".into()),
            ("authorization".into(), "Bearer target".into()),
            ("x-custom".into(), "kept".into()),
        ])
    );
}
