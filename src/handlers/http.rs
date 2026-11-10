use crate::models::Problem;
use crate::{
    db,
    features::schedules::JobSchedule,
    models::{
        AppState, Error, HttpMeta, JobCreate, JobCreateRow, JobMeta, JobRetry, JobSink, ParamError,
        WorkerOptions, headers::forwarded_headers,
    },
    otel::{self, metrics::metrics},
};
use axum::{
    Router,
    body::Bytes,
    extract::{DefaultBodyLimit, Path, RawQuery, State},
    http::{HeaderMap, HeaderValue, StatusCode},
    response::IntoResponse,
    routing::get,
};
use chrono::{DateTime, Utc};
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
                .patch(job_create)
                .layer(DefaultBodyLimit::max(state.max_body_size)),
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
    let job =
        prepare_job(&state.worker_options, request, Utc::now()).map_err(Error::InvalidParams)?;
    let job = create_job(&state, job).await?;
    Ok((StatusCode::CREATED, job_headers(&job)))
}

/// The `Location` and `Irisqo-*` headers of a created job.
pub fn job_headers(job: &JobCreateRow) -> HeaderMap {
    let mut headers = HeaderMap::new();
    // schedule-id and key (user-provided) may not be valid header values
    let mut put = |name: &'static str, value: Option<&str>| {
        if let Some(value) = value
            && let Ok(value) = HeaderValue::from_str(value)
        {
            headers.insert(name, value);
        }
    };
    put("location", Some(&format!("/api/v1/jobs/{}", job.id)));
    put("irisqo-job-id", Some(&job.id.to_string()));
    put("irisqo-schedule-id", job.schedule_id.as_deref());
    put("irisqo-key", job.key.as_deref());
    headers
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
    now: DateTime<Utc>,
) -> Result<JobCreate, Vec<ParamError>> {
    let mut errors = Vec::new();
    let mut at: Option<DateTime<Utc>> = None;
    let mut timeout: Option<u32> = None;
    let mut retry = JobRetry::None;
    let mut schedule: Option<(JobSchedule, &'static str)> = None;
    let mut until: Option<DateTime<Utc>> = None;
    let mut key: Option<String> = None;
    let mut tz: Option<chrono_tz::Tz> = None;
    let mut url = Url::parse(request.url).ok();

    let mut bad = |parameter, detail: &str| errors.push(ParamError::new(parameter, detail));
    for (name, value) in form_urlencoded::parse(request.query.unwrap_or_default().as_bytes()) {
        match name.as_ref() {
            "_delay" => match value.parse::<u32>() {
                Ok(secs) => at = Some(now + chrono::Duration::seconds(i64::from(secs))),
                Err(_) => bad("_delay", "must be a number of seconds"),
            },
            // a time in the past runs the job now
            "_delay_until" => match JobSchedule::parse_time(&value) {
                Some(t) => at = (t > now).then_some(t),
                None => bad("_delay_until", TIME_RULE),
            },
            "_timeout" => match value.parse::<u32>() {
                Ok(ms) if ms > 0 => timeout = Some(ms.min(options.max_timeout)),
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
            "_cron" => match JobSchedule::parse_cron(&value) {
                Ok(parsed) => schedule = Some((parsed, "_cron")),
                Err(err) => bad("_cron", &err.detail),
            },
            "_tz" => match value.parse() {
                Ok(parsed) => tz = Some(parsed),
                Err(_) => bad("_tz", TZ_RULE),
            },
            "_until" => match JobSchedule::parse_time(&value) {
                Some(t) => until = Some(t),
                None => bad("_until", TIME_RULE),
            },
            "_key" if !value.is_empty() && !is_key(&value) => bad("_key", KEY_RULE),
            "_key" => key = (!value.is_empty()).then(|| value.into_owned()),
            _ => {
                if let Some(url) = url.as_mut() {
                    if value.is_empty() {
                        url.query_pairs_mut().append_key_only(&name);
                    } else {
                        url.query_pairs_mut().append_pair(&name, &value);
                    }
                }
            }
        }
    }
    if tz.is_some() && !matches!(schedule, Some((JobSchedule::Cron { .. }, _))) {
        errors.push(ParamError::new("_tz", "applies to _cron only"));
    }
    // the zone belongs to the cron expression
    let schedule =
        schedule.map(|(schedule, parameter)| (schedule.in_zone(tz.unwrap_or_default()), parameter));
    // a schedule runs at least once
    if let Some((schedule, parameter)) = &schedule
        && schedule.next(at.unwrap_or(now), until).is_none()
    {
        let parameter = if until.is_some() { "_until" } else { parameter };
        errors.push(ParamError::new(
            parameter,
            "the schedule has no run before it ends",
        ));
    }
    let sink = url
        .and_then(|url| Uri::try_from(url.as_str()).ok())
        .map(|url| {
            JobSink::Http(HttpMeta {
                method: request.method,
                url,
            })
        });
    match sink.as_ref().map(|sink| sink.check(options)) {
        None => errors.push(ParamError::new("url", "is not a valid URL")),
        Some(Err(detail)) => errors.push(ParamError::new("url", detail)),
        Some(Ok(())) => {}
    }
    let Some(sink) = sink.filter(|_| errors.is_empty()) else {
        return Err(errors);
    };
    // Trace context of this request: the parent of every attempt
    let (traceparent, tracestate) = otel::current_trace_headers();
    Ok(JobCreate {
        meta: JobMeta {
            sink,
            retry,
            delay: at.and_then(|t| u32::try_from((t - now).num_seconds()).ok()),
            timeout,
            traceparent,
            tracestate,
        },
        headers: Some(request.headers),
        body: request.body,
        at,
        schedule: schedule.map(|(schedule, _)| schedule),
        until,
        key,
    })
}

/// Stores a prepared job (or returns the existing one with the same key).
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

/// What is wrong with a time that [`JobSchedule::parse_time`] refuses.
pub(crate) const TIME_RULE: &str = "must be an RFC 3339 time or unix seconds";

/// What is wrong with a time zone name (`_tz`, `tz`).
pub(crate) const TZ_RULE: &str = "must be an IANA time zone name, e.g. Europe/Berlin";

/// A JSON request body; one that is not valid is a 400 with the reason.
pub(crate) fn parse_body<T: serde::de::DeserializeOwned>(
    body: &[u8],
    what: &str,
) -> Result<T, Error> {
    serde_json::from_slice(body)
        .map_err(|err| Error::invalid("body", format!("is not a valid {what}: {err}")))
}

/// What is wrong with a key that [`is_key`] refuses.
pub(crate) const KEY_RULE: &str = "must be 1 to 255 characters without control characters";

/// An idempotency key (as in `enqueue_v1`): 1 to 255 characters, no control characters.
pub(crate) fn is_key(value: &str) -> bool {
    !value.is_empty() && value.chars().count() <= 255 && !value.chars().any(char::is_control)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_target_is_an_http_url() {
        let options = WorkerOptions::for_tests();
        let prepare = |url| {
            prepare_job(
                &options,
                JobRequest {
                    method: Method::GET,
                    url,
                    query: None,
                    headers: HashMap::new(),
                    body: Bytes::new(),
                },
                Utc::now(),
            )
        };
        assert!(prepare("https://example.com/hook").is_ok());
        for url in ["ftp://example.com", "mailto:a@example.com", "not a url"] {
            let errors = prepare(url).expect_err(url);
            assert_eq!(errors.len(), 1, "{url}");
            assert_eq!(errors[0].parameter, "url", "{url}");
        }
    }

    #[test]
    fn tz_is_the_zone_of_cron() {
        let options = WorkerOptions::for_tests();
        let prepare = |query| {
            prepare_job(
                &options,
                JobRequest {
                    method: Method::GET,
                    url: "https://example.com",
                    query: Some(query),
                    headers: HashMap::new(),
                    body: Bytes::new(),
                },
                Utc::now(),
            )
        };
        let job = prepare("_cron=0+9+*+*+*&_tz=Europe/Berlin").expect("valid");
        assert_eq!(
            job.schedule.map(|s| s.tz()),
            Some(chrono_tz::Europe::Berlin)
        );
        assert_eq!(
            prepare("_cron=0+9+*+*+*")
                .expect("valid")
                .schedule
                .map(|s| s.tz()),
            Some(chrono_tz::Tz::UTC)
        );
        for query in [
            "_interval=60&_tz=Europe/Berlin",
            "_tz=Europe/Berlin",
            "_cron=0+9+*+*+*&_tz=Mars/Base",
        ] {
            let errors = prepare(query).expect_err(query);
            assert_eq!(errors.len(), 1, "{query}");
            assert_eq!(errors[0].parameter, "_tz", "{query}");
        }
    }
}
