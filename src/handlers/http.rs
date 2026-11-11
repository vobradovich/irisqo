use crate::models::Problem;
use crate::{
    db,
    features::schedules::{JobSchedule, ScheduleRule},
    handlers::job_request::{JobRequest, SinkKind},
    models::{
        AppState, Error, JobCreate, JobCreateRow, ParamError, WorkerOptions,
        headers::forwarded_headers,
    },
    otel::metrics::metrics,
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
use hyper::Method;
use opentelemetry::KeyValue;

use std::sync::Arc;
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
    let request = JobRequest::http(&method, &url, forwarded_headers(&headers), body);
    let job = prepare_job(&state.worker_options, request, query.as_deref(), Utc::now())
        .map_err(Error::InvalidParams)?;
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

/// Validates a job requested through `/to/{url}?{query}` (`input` is [`JobRequest::http`], the UI
/// form builds the same) and builds the job: every invalid parameter is reported, not only the
/// first one. `_*` parameters are the job's, the others stay on the URL.
pub fn prepare_job(
    options: &WorkerOptions,
    mut input: JobRequest,
    query: Option<&str>,
    now: DateTime<Utc>,
) -> Result<JobCreate, Vec<ParamError>> {
    let mut errors = Vec::new();
    let mut rule: Option<(ScheduleRule, &'static str)> = None;
    let mut until: Option<DateTime<Utc>> = None;
    let mut tz: Option<chrono_tz::Tz> = None;
    let mut url = input.url.as_deref().and_then(|url| Url::parse(url).ok());

    let mut bad = |parameter, detail: &str| errors.push(ParamError::new(parameter, detail));
    for (name, value) in form_urlencoded::parse(query.unwrap_or_default().as_bytes()) {
        match name.as_ref() {
            "_interval" => match value.parse() {
                Ok(parsed @ ScheduleRule::Interval(_)) => rule = Some((parsed, "_interval")),
                _ => bad("_interval", "must be a positive number of seconds"),
            },
            "_cron" => match ScheduleRule::parse_cron(&value) {
                Ok(parsed) => rule = Some((parsed, "_cron")),
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
            _ => match input.set_param(&name, &value) {
                Ok(true) => {}
                Err(err) => bad(err.parameter, &err.detail),
                // the target's own query parameter
                Ok(false) => {
                    if let Some(url) = url.as_mut() {
                        if value.is_empty() {
                            url.query_pairs_mut().append_key_only(&name);
                        } else {
                            url.query_pairs_mut().append_pair(&name, &value);
                        }
                    }
                }
            },
        }
    }
    if until.is_some() && rule.is_none() {
        errors.push(ParamError::new(
            "_until",
            "applies to _cron and _interval only",
        ));
    }
    let rule_parameter = rule.as_ref().map(|(_, parameter)| *parameter);
    let schedule = rule.map(|(rule, _)| JobSchedule {
        rule,
        tz: tz.unwrap_or_default(),
        until,
    });
    // `noop://…` is the `noop` sink: nothing is called
    if url.as_ref().is_some_and(|url| url.scheme() == "noop") {
        input.sink = Some(SinkKind::Noop);
    }
    // a URL that does not parse stays empty: `validate` says it is not a valid one
    input.url = Some(url.map(String::from).unwrap_or_default());
    let job = input
        .validate(options, now)
        .map_err(|found| {
            errors.extend(
                found
                    .into_iter()
                    .map(|err| ParamError::new(query_parameter(err.parameter), err.detail)),
            );
        })
        .ok()
        .map(|job| JobCreate { schedule, ..job });
    // a schedule runs at least once
    if let Some(job) = &job
        && let Some((schedule, parameter)) = job.schedule.as_ref().zip(rule_parameter)
        && schedule.next(job.at.unwrap_or(now)).is_none()
    {
        let parameter = if until.is_some() { "_until" } else { parameter };
        errors.push(ParamError::new(
            parameter,
            "the schedule has no run before it ends",
        ));
    }
    job.filter(|_| errors.is_empty()).ok_or(errors)
}

/// The job parameters of `/to/`: `_` and the name of the field of [`JobRequest`].
const JOB_PARAMETERS: [&str; 6] = ["_delay", "_at", "_timeout", "_queue", "_priority", "_key"];

/// The `/to/` parameter of a job field; the field itself when it has none (`url`, `headers`).
fn query_parameter(field: &'static str) -> &'static str {
    JOB_PARAMETERS
        .into_iter()
        .find(|parameter| parameter.strip_prefix('_') == Some(field))
        .unwrap_or(field)
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
    use std::collections::HashMap;

    fn prepare_query(query: &str) -> Result<JobCreate, Vec<ParamError>> {
        prepare_job(
            &WorkerOptions::for_tests(),
            JobRequest::http(
                &Method::GET,
                "https://example.com",
                HashMap::new(),
                Bytes::new(),
            ),
            Some(query),
            Utc::now(),
        )
    }

    #[test]
    fn until_belongs_to_a_schedule() {
        let until = JobSchedule::parse_time("2100-01-01T00:00:00Z");
        for query in ["_interval=60", "_cron=0+9+*+*+*"] {
            let job =
                prepare_query(&format!("{query}&_until=2100-01-01T00:00:00Z")).expect("valid");
            assert_eq!(job.schedule.and_then(|s| s.until), until, "{query}");
        }
        // an end without a schedule is not a job that ends
        let errors = prepare_query("_until=2100-01-01T00:00:00Z").expect_err("no schedule");
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].parameter, "_until");
    }

    #[test]
    fn queue_and_priority_are_the_jobs() {
        let meta = |query| prepare_query(query).expect("valid").meta;
        let m = meta("_queue=order-42");
        assert_eq!((m.queue.as_deref(), m.priority), (Some("order-42"), None));
        let m = meta("_priority=5");
        assert_eq!((m.queue, m.priority), (None, Some(5)));
        assert_eq!(meta("_queue=").queue, None);
        for (query, parameter) in [
            ("_queue=Bad+Name", "_queue"),
            ("_priority=high", "_priority"),
            ("_queue=a&_priority=1", "_priority"),
        ] {
            let errors = prepare_query(query).expect_err(query);
            assert_eq!(errors.len(), 1, "{query}");
            assert_eq!(errors[0].parameter, parameter, "{query}");
        }
    }

    #[test]
    fn noop_is_a_target_scheme() {
        let options = WorkerOptions::for_tests();
        let prepare = |url| {
            let request = JobRequest::http(&Method::POST, url, HashMap::new(), Bytes::new());
            prepare_job(&options, request, None, Utc::now())
        };
        let job = prepare("noop://bench").expect("valid");
        assert_eq!(job.meta.sink, crate::models::JobSink::Noop);
        assert!(prepare("none://bench").is_err(), "an unknown scheme");
    }

    #[test]
    fn a_target_is_an_http_url() {
        let options = WorkerOptions::for_tests();
        let prepare = |url| {
            prepare_job(
                &options,
                JobRequest::http(&Method::GET, url, HashMap::new(), Bytes::new()),
                None,
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
        let zone = |query| prepare_query(query).expect("valid").schedule.map(|s| s.tz);
        assert_eq!(
            zone("_cron=0+9+*+*+*&_tz=Europe/Berlin"),
            Some(chrono_tz::Europe::Berlin)
        );
        assert_eq!(zone("_cron=0+9+*+*+*"), Some(chrono_tz::Tz::UTC));
        let errors = prepare_query("_cron=0+9+*+*+*&_tz=Mars/Base").expect_err("no such zone");
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].parameter, "_tz");
    }
}
