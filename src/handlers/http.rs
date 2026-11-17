use crate::models::Problem;
use crate::{
    db,
    handlers::job_request::JobRequest,
    models::{
        AppState, Error, JobCallback, JobCreate, JobCreateRow, ParamError, WorkerOptions,
        headers::{callback_headers, forwarded_headers},
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
use hyper::{Method, Uri};
use opentelemetry::KeyValue;

use std::{borrow::Cow, collections::HashMap, sync::Arc};
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
    let url = Uri::try_from(url).map_err(|_| Error::invalid("url", "is not a valid URL"))?;
    let request = JobRequest::http(&method, url, forwarded_headers(&headers), body);
    let job = prepare_job(
        &state.worker_options,
        request,
        form_urlencoded::parse(query.unwrap_or_default().as_bytes()),
        callback_headers(&headers),
        Utc::now(),
    )
    .map_err(Error::InvalidParams)?;
    let (job, status) = create_job(&state, job).await?;
    Ok((status, job_headers(&job)))
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
/// form builds the same; `params` are the pairs of the query) and builds the job: every invalid
/// parameter is reported, not only the first one, named as the field of JSON (`delay`,
/// `callback.url`). The `_*` parameters of the job and of its callback are theirs, the others
/// stay on the URL. `callback_headers` are the `Irisqo-Callback-*` headers, which go to the
/// callback.
pub fn prepare_job<'a>(
    options: &WorkerOptions,
    input: JobRequest,
    params: impl IntoIterator<Item = (Cow<'a, str>, Cow<'a, str>)>,
    callback_headers: HashMap<String, String>,
    now: DateTime<Utc>,
) -> Result<JobCreate, Vec<ParamError>> {
    let mut url = Url::parse(&input.url.to_string()).ok();
    let (mut job, mut callback) = (HashMap::new(), HashMap::new());
    for (name, value) in params {
        let field =
            name.strip_prefix('_')
                .and_then(|param| match param.strip_prefix("callback_") {
                    Some(field) if JobCallback::PARAMS.contains(&field) => {
                        Some((&mut callback, field))
                    }
                    _ if JobRequest::PARAMS.contains(&param) => Some((&mut job, param)),
                    _ => None,
                });
        match (field, url.as_mut()) {
            (Some((params, field)), _) => _ = params.insert(field.to_owned(), value.into_owned()),
            // the target's own query parameter
            (None, Some(url)) if value.is_empty() => {
                _ = url.query_pairs_mut().append_key_only(&name)
            }
            (None, Some(url)) => _ = url.query_pairs_mut().append_pair(&name, &value),
            (None, None) => {}
        }
    }

    let (mut input, mut errors) = input.with_params(&job);
    input.callback =
        JobCallback::from_params(&callback, callback_headers).unwrap_or_else(|found| {
            errors.extend(found);
            None
        });
    if let Some(url) = url.and_then(|url| Uri::try_from(url.as_str()).ok()) {
        input.url = url;
    }
    let job = input
        .validate(options, now)
        .map_err(|found| errors.extend(found))
        .ok();
    job.filter(|_| errors.is_empty()).ok_or(errors)
}

/// Stores a prepared job (or returns the existing one with the same key): `201` for a new job or
/// schedule, `200` for the existing job or an updated schedule.
pub async fn create_job(
    state: &AppState,
    job: JobCreate,
) -> Result<(JobCreateRow, StatusCode), Error> {
    debug!(sink = ?job.meta.sink, "create job");
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
        .map(|(row, created)| {
            let status = if created {
                StatusCode::CREATED
            } else {
                StatusCode::OK
            };
            (row, status)
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
    use crate::features::schedules::JobSchedule;
    use crate::models::{CallbackType, FlowKey, JobRetry};

    fn prepare_query(query: &str) -> Result<JobCreate, Vec<ParamError>> {
        prepare_job(
            &WorkerOptions::for_tests(),
            JobRequest::http(
                &Method::GET,
                "https://example.com".parse().unwrap(),
                HashMap::new(),
                Bytes::new(),
            ),
            form_urlencoded::parse(query.as_bytes()),
            HashMap::new(),
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
        assert_eq!(errors[0].parameter, "until");
    }

    #[test]
    fn flow_and_priority_are_the_jobs() {
        let meta = |query| prepare_query(query).expect("valid").meta;
        let m = meta("_flow_key=order-42");
        assert_eq!(
            (m.flow_key.as_ref().map(FlowKey::as_str), m.priority),
            (Some("order-42"), None)
        );
        let m = meta("_priority=5");
        assert_eq!((m.flow_key, m.priority), (None, Some(5)));
        assert_eq!(meta("_flow_key=").flow_key, None);
        for (query, parameter) in [
            ("_flow_key=Bad+Name", "flow_key"),
            ("_priority=high", "priority"),
        ] {
            let errors = prepare_query(query).expect_err(query);
            assert_eq!(errors.len(), 1, "{query}");
            assert_eq!(errors[0].parameter, parameter, "{query}");
        }
    }

    #[test]
    fn callback_parameters_make_the_callback() {
        let callback = prepare_query(
            "_callback_url=https%3A%2F%2Fme.example%2Fcb&_callback_method=put&_callback_retry=none&_callback_priority=3&_callback_types=irisqo.job.failed&_callback_timeout=500",
        )
        .expect("valid")
        .meta
        .callback
        .expect("callback");
        assert_eq!(callback.url.to_string(), "https://me.example/cb");
        assert_eq!(callback.method, Some(Method::PUT));
        assert_eq!(callback.retry, Some(JobRetry::NONE));
        assert_eq!(callback.priority, Some(3));
        assert_eq!(callback.types, Some(vec![CallbackType::Failed]));
        assert_eq!(callback.timeout.map(std::num::NonZeroU32::get), Some(500));
        // none of them: no callback, and the callback parameters are not the target's
        for query in ["", "_callback_url="] {
            let job = prepare_query(query).expect("valid");
            assert!(job.meta.callback.is_none(), "{query}");
        }
        // the headers go to the callback, and need its url
        let prepare = |query: &str| {
            let request = JobRequest::http(
                &Method::GET,
                "https://example.com".parse().unwrap(),
                HashMap::new(),
                Bytes::new(),
            );
            let headers = HashMap::from([("x-own".to_owned(), "1".to_owned())]);
            prepare_job(
                &WorkerOptions::for_tests(),
                request,
                form_urlencoded::parse(query.as_bytes()),
                headers,
                Utc::now(),
            )
        };
        let job = prepare("_callback_url=https%3A%2F%2Fme.example").expect("valid");
        assert_eq!(
            job.meta.callback.and_then(|c| c.headers).map(|h| h.len()),
            Some(1)
        );
        assert_eq!(
            prepare("").expect_err("no url")[0].parameter,
            "callback.url"
        );
        let url = "_callback_url=https%3A%2F%2Fme.example";
        for (query, parameter) in [
            ("_callback_mode=relay".to_owned(), "callback.url"),
            (format!("{url}&_callback_mode=final"), "callback.mode"),
            (format!("{url}&_callback_method=GET"), "callback.method"),
            (format!("{url}&_callback_types=completed"), "callback.types"),
            (
                format!("{url}&_callback_retry=3|fibonacci"),
                "callback.retry",
            ),
            (format!("{url}&_callback_timeout=0"), "callback.timeout"),
            (format!("{url}&_callback_flow_key=Bad"), "callback.flow_key"),
            (
                "_callback_url=ftp%3A%2F%2Fme.example".to_owned(),
                "callback.url",
            ),
        ] {
            let errors = prepare_query(&query).expect_err(&query);
            assert_eq!(errors[0].parameter, parameter, "{query}");
        }
    }

    #[test]
    fn every_listed_parameter_is_parsed() {
        // a name routed to a parser that does not read it would be an error of its own
        let all = |names: &[&str], prefix: &str| {
            names
                .iter()
                .map(|name| format!("_{prefix}{name}=x"))
                .collect::<Vec<_>>()
                .join("&")
        };
        let query = format!(
            "{}&{}",
            all(&JobRequest::PARAMS, ""),
            all(&JobCallback::PARAMS, "callback_")
        );
        let errors = prepare_query(&query).expect_err("values that do not parse");
        assert!(
            errors.iter().all(|e| e.detail != "is not a parameter"),
            "{errors:?}"
        );
    }

    #[test]
    fn noop_is_a_target_scheme() {
        let options = WorkerOptions::for_tests();
        let prepare = |url: &str| {
            let request = JobRequest::http(
                &Method::POST,
                url.parse().unwrap(),
                HashMap::new(),
                Bytes::new(),
            );
            prepare_job(&options, request, [], HashMap::new(), Utc::now())
        };
        let job = prepare("noop://bench").expect("valid");
        assert_eq!(job.meta.sink, crate::models::JobSink::Noop);
        assert!(prepare("none://bench").is_err(), "an unknown scheme");
    }

    #[test]
    fn a_target_is_an_http_url() {
        let options = WorkerOptions::for_tests();
        let prepare = |url: &str| {
            prepare_job(
                &options,
                JobRequest::http(
                    &Method::GET,
                    url.parse().unwrap(),
                    HashMap::new(),
                    Bytes::new(),
                ),
                [],
                HashMap::new(),
                Utc::now(),
            )
        };
        assert!(prepare("https://example.com/hook").is_ok());
        assert!(Uri::try_from("not a url").is_err());
        for url in ["ftp://example.com", "mailto:a@example.com"] {
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
            Some(jiff::tz::TimeZone::get("Europe/Berlin").unwrap())
        );
        assert_eq!(zone("_cron=0+9+*+*+*"), Some(jiff::tz::TimeZone::UTC));
        let errors = prepare_query("_cron=0+9+*+*+*&_tz=Mars/Base").expect_err("no such zone");
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].parameter, "tz");
    }
}
