use crate::models::Problem;
use crate::{
    db,
    features::schedules::JobSchedule,
    models::{
        AppState, Error, HttpMeta, JobCreate, JobCreateRow, JobMeta, JobProtocol, JobRetry,
        ParamError, WorkerOptions, headers::forwarded_headers,
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
        .map_err(Error::InvalidParams)?;
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
    let mut at: Option<i64> = None;
    let mut timeout: u32 = options.timeout;
    let mut retry = JobRetry::None;
    let mut schedule: Option<(JobSchedule, &'static str)> = None;
    let mut until: Option<i64> = None;
    let mut external_id: Option<String> = None;
    let mut url = Url::parse(request.url).ok();

    let mut bad = |parameter, detail: &str| errors.push(ParamError::new(parameter, detail));
    for (key, value) in form_urlencoded::parse(request.query.unwrap_or_default().as_bytes()) {
        match key.as_ref() {
            "_delay" => match value.parse::<u32>() {
                Ok(secs) => at = Some(now_secs + i64::from(secs)),
                Err(_) => bad("_delay", "must be a number of seconds"),
            },
            // a time in the past runs the job now
            "_delay_until" => match value.parse::<i64>() {
                Ok(t) if JobSchedule::is_valid_unix_secs(t) => at = (t > now_secs).then_some(t),
                _ => bad("_delay_until", "must be a unix time in seconds"),
            },
            "_timeout" => match value.parse::<u32>() {
                Ok(ms) if ms > 0 => timeout = ms.min(options.max_timeout),
                _ => bad("_timeout", "must be a positive number of milliseconds"),
            },
            "_retry" => match value.parse::<JobRetry>() {
                Ok(parsed) => retry = parsed,
                Err(_) => bad("_retry", "must be N, N|fixed|S or N|fibonacci|S"),
            },
            "_interval" => match value.parse() {
                Ok(parsed @ JobSchedule::Interval { .. }) => schedule = Some((parsed, "_interval")),
                _ => bad("_interval", "must be a positive number of seconds"),
            },
            "_cron" => match value.parse() {
                Ok(parsed @ JobSchedule::Cron { .. }) => schedule = Some((parsed, "_cron")),
                Ok(JobSchedule::Interval { .. }) => bad("_cron", "is not a cron expression"),
                Err(err) => bad("_cron", &err.detail),
            },
            "_until" => match value.parse::<i64>() {
                Ok(t) if JobSchedule::is_valid_unix_secs(t) => until = Some(t),
                _ => bad("_until", "must be a unix time in seconds"),
            },
            "_id" if value.chars().count() > 64 => bad("_id", "must be at most 64 characters"),
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
    // a schedule runs at least once
    if let Some((schedule, parameter)) = &schedule
        && schedule.next(at.unwrap_or(now_secs), until).is_none()
    {
        let parameter = if until.is_some() { "_until" } else { parameter };
        errors.push(ParamError::new(
            parameter,
            "the schedule has no run before it ends",
        ));
    }
    let uri = url.and_then(|url| Uri::try_from(url.as_str()).ok());
    match uri.as_ref() {
        None => errors.push(ParamError::new("url", "is not a valid URL")),
        Some(uri) => {
            // a private IP address or localhost; a name is checked when resolved
            if let Some(Err(detail)) = uri.host().map(|host| options.targets.check_host(host)) {
                errors.push(ParamError::new("url", detail));
            }
        }
    }
    let Some(uri) = uri.filter(|_| errors.is_empty()) else {
        return Err(errors);
    };
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
            delay: at.and_then(|t| u32::try_from(t - now_secs).ok()),
            timeout,
            traceparent,
            tracestate,
        },
        headers: Some(request.headers),
        body: request.body,
        at,
        schedule: schedule.map(|(schedule, _)| schedule),
        until,
        external_id,
    })
}

/// Stores a prepared job (or returns the existing one with the same external id).
pub async fn create_job(state: &AppState, job: JobCreate) -> Result<JobCreateRow, Error> {
    debug!(meta = ?job.meta, "create job");
    let job_type = if job.schedule.is_some() {
        "scheduled"
    } else if job.at.is_some() {
        "delayed"
    } else {
        "immediate"
    };
    db::jobqueue::create(&state.pool, job, &state.instance_id)
        .await
        .inspect(|_| {
            metrics()
                .jobs_created
                .add(1, &[KeyValue::new("job.type", job_type)]);
        })
}
