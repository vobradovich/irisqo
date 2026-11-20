use super::http::{KEY_RULE, TIME_RULE, TZ_RULE, is_key};
use crate::features::schedules::{JobSchedule, ScheduleRule};
use crate::models::headers::{is_reserved_callback_header, is_reserved_header};
use crate::models::{
    FlowKey, HttpMeta, JobCallback, JobCreate, JobMeta, JobRetry, JobSink, MILLISECONDS_RULE,
    PRIORITY_RULE, ParamError, Params, TargetPolicy, WorkerOptions, deserialize_method_of,
    deserialize_uri_of,
};
use axum::body::Bytes;
use base64::Engine as _;
use chrono::{DateTime, Utc};
use hyper::{
    Method, Uri,
    header::{HeaderName, HeaderValue},
};
use jiff::tz::TimeZone;
use serde::{Deserialize, Deserializer, de};
use serde_json::value::RawValue;
use std::{collections::HashMap, num::NonZeroU32};
use utoipa::{
    ToSchema,
    openapi::schema::{KnownFormat, ObjectBuilder, OneOfBuilder, SchemaFormat, Type},
};

/// JSON job format v1, as `enqueue_v1` takes it, the `/to/` query and the "New job" form: the one
/// description of a job that every way to create one fills (JSON by serde, `/to/` and the form
/// by [`JobRequest::with_params`]) and [`JobRequest::validate`] checks.
///
/// `url` (an `http` job, or `noop://…`: the `noop` sink) with `method` (`POST`), `retry` (an
/// object, `policy` of `fixed`, `fibonacci`, `exponential` with `count`, `delay`, which `fixed`
/// may leave out to retry at once, and `max_delay` of the last two; or its text,
/// `5|exponential|2`), `callback` (an object, see [`JobCallback`]), `delay` (seconds) or `at`,
/// `timeout` (milliseconds), `flow_key`, `priority`, `headers`, `body` or `body_base64`, `key`;
/// `cron` or `interval` with `tz` (of `cron`) and `until` repeat the job.
/// The retry policy defaults to none, the timeout to `--timeout` when the job runs. Fields that
/// are not known are ignored.
#[derive(Debug, Deserialize, ToSchema)]
pub struct JobRequest {
    /// An HTTP method, for an `http` url, in any case (`post` is `POST`, as in `enqueue_v1`);
    /// `POST` when absent.
    #[serde(default, deserialize_with = "deserialize_method")]
    #[schema(value_type = Option<String>)]
    pub(crate) method: Option<Method>,
    /// An `http` or `https` URL; `noop://…` is the `noop` sink. Required.
    #[serde(deserialize_with = "deserialize_url")]
    #[schema(value_type = String)]
    pub(crate) url: Uri,
    /// The retry policy, the object or its text (`5|exponential|2`); no retries when absent.
    #[serde(default, deserialize_with = "JobRetry::deserialize_optional")]
    pub(crate) retry: Option<JobRetry>,
    /// Milliseconds; `--timeout` when absent.
    #[schema(value_type = Option<u32>, minimum = 1)]
    pub(crate) timeout: Option<NonZeroU32>,
    /// Flow key of the job: 1 to 128 characters of `a-z 0-9 . _ : -`; an empty one is none.
    #[serde(default, deserialize_with = "FlowKey::deserialize_optional")]
    pub(crate) flow_key: Option<FlowKey>,
    /// Higher runs first, among the jobs of its flow if it has one.
    pub(crate) priority: Option<i32>,
    /// Headers for the target. Names that differ only by case are rejected.
    #[schema(value_type = Option<HashMap<String, String>>)]
    pub(crate) headers: Option<HashMap<String, String>>,
    /// A string is sent as it is, any other JSON value as JSON text (`content-type` defaults to JSON).
    #[schema(value_type = Option<Object>)]
    pub(crate) body: Option<Box<RawValue>>,
    /// Base64 of a binary body, not together with `body`.
    pub(crate) body_base64: Option<String>,
    /// When the job runs: an RFC 3339 time (a string) or unix seconds (a number); not together
    /// with `delay`.
    #[serde(default, deserialize_with = "at")]
    #[schema(schema_with = at_schema)]
    pub(crate) at: Option<DateTime<Utc>>,
    /// Seconds from now; not together with `at`.
    pub(crate) delay: Option<u32>,
    /// A repeated key returns the existing job. With `cron` or `interval` it is the id of the
    /// schedule (a new one when absent), and a repeated key changes the schedule.
    pub(crate) key: Option<String>,
    /// Repeats the job: a cron expression of 5 fields (or 6 with seconds first), or `interval`,
    /// not both.
    pub(crate) cron: Option<String>,
    /// Repeats the job every so many seconds.
    pub(crate) interval: Option<u32>,
    /// The IANA zone of `cron` (`Europe/Berlin`), default UTC; only with `cron` or `interval`.
    #[serde(default, deserialize_with = "zone")]
    #[schema(value_type = Option<String>)]
    pub(crate) tz: Option<TimeZone>,
    /// When the schedule ends, as `at`: no run after it.
    #[serde(default, deserialize_with = "at")]
    #[schema(schema_with = at_schema)]
    pub(crate) until: Option<DateTime<Utc>>,
    /// A request that irisqo makes when the job reaches its outcome. Its `url` is required.
    pub(crate) callback: Option<JobCallback>,
    /// The body as it came (`/to/`, the form): it is sent as it is, `body` is not looked at.
    #[serde(skip)]
    pub(crate) body_raw: Option<Bytes>,
}

fn deserialize_url<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Uri, D::Error> {
    deserialize_uri_of("url", deserializer)
}

fn deserialize_method<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Method>, D::Error> {
    deserialize_method_of("method", deserializer)
}

/// An RFC 3339 time or unix seconds.
fn at_schema() -> utoipa::openapi::Schema {
    let time = ObjectBuilder::new()
        .schema_type(Type::String)
        .format(Some(SchemaFormat::KnownFormat(KnownFormat::DateTime)))
        .examples(["2030-01-01T00:00:00Z"]);
    let seconds = ObjectBuilder::new()
        .schema_type(Type::Integer)
        .format(Some(SchemaFormat::KnownFormat(KnownFormat::Int64)))
        .examples([1_893_456_000]);
    utoipa::openapi::Schema::OneOf(OneOfBuilder::new().item(time).item(seconds).build())
}

/// `tz`: an IANA zone name (`Europe/Berlin`). A fixed offset is not one: it does not follow
/// the clock changes of a place.
fn zone<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<TimeZone>, D::Error> {
    Option::<String>::deserialize(deserializer)?
        .map(|name| TimeZone::get(&name).map_err(|_| de::Error::custom(TZ_RULE)))
        .transpose()
}

/// `at`: an RFC 3339 time (a string) or unix seconds (a number).
fn at<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<DateTime<Utc>>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum At {
        Seconds(i64),
        Time(String),
    }
    let time = match Option::<At>::deserialize(deserializer)? {
        None => return Ok(None),
        Some(At::Seconds(secs)) => JobSchedule::from_unix_secs(secs),
        Some(At::Time(time)) => JobSchedule::parse_time(&time),
    };
    time.map(Some).ok_or_else(|| de::Error::custom(TIME_RULE))
}

impl JobRequest {
    /// An `http` job for `/to/` and the form: the method, the headers to send, the body as it is.
    /// The target is not set yet: [`prepare_job`](super::http::prepare_job) parses it.
    pub(crate) fn http(method: &Method, headers: HashMap<String, String>, body: Bytes) -> Self {
        // every field spelled out: a new one must be decided here too
        Self {
            method: Some(method.clone()),
            url: Uri::default(),
            retry: None,
            timeout: None,
            flow_key: None,
            priority: None,
            headers: Some(headers),
            body: None,
            body_base64: None,
            at: None,
            delay: None,
            key: None,
            cron: None,
            interval: None,
            tz: None,
            until: None,
            callback: None,
            body_raw: Some(body),
        }
    }

    /// The fields of a job that come as text, one by one: the `_*` parameters of `/to/` without
    /// their `_`.
    pub(crate) const PARAMS: [&str; 11] = [
        "delay", "at", "timeout", "retry", "flow_key", "priority", "key", "cron", "interval", "tz",
        "until",
    ];

    /// The request with the fields of [`Self::PARAMS`] in `params` (`/to/` sets nothing else of
    /// them), and the errors of the ones that do not parse, named by their field. The fields are
    /// checked by [`Self::validate`].
    #[must_use = "the errors of the parameters that do not parse must be reported"]
    pub(crate) fn with_params(self, params: &HashMap<String, String>) -> (Self, Vec<ParamError>) {
        let mut p = Params::new(params, "");
        let request = Self {
            delay: p.parse("delay", "must be a number of seconds", |v| v.parse().ok()),
            at: p.parse("at", TIME_RULE, JobSchedule::parse_time),
            timeout: p.parse("timeout", MILLISECONDS_RULE, |v| v.parse().ok()),
            retry: p
                .parse("retry", JobRetry::RULE, |v| JobRetry::parse(v).ok())
                .flatten(),
            flow_key: p
                .parse("flow_key", FlowKey::RULE, |v| {
                    FlowKey::parse_optional(v).ok()
                })
                .flatten(),
            priority: p.parse("priority", PRIORITY_RULE, |v| v.parse().ok()),
            key: p
                .raw("key")
                .filter(|key| !key.is_empty())
                .map(str::to_owned),
            cron: p.raw("cron").map(str::to_owned),
            interval: p.parse("interval", ScheduleRule::INTERVAL_RULE, |v| v.parse().ok()),
            tz: p.parse("tz", TZ_RULE, |v| TimeZone::get(v).ok()),
            until: p.parse("until", TIME_RULE, JobSchedule::parse_time),
            ..self
        };
        (request, p.into_errors())
    }

    /// Validates the description of a job: every invalid field is reported, not only the first.
    /// The `parameter` of an error is the name of the field.
    pub(crate) fn validate(
        mut self,
        options: &WorkerOptions,
        targets: &TargetPolicy,
        now: DateTime<Utc>,
    ) -> Result<JobCreate, Vec<ParamError>> {
        let mut errors = Errors::default();

        // `noop://…` is the `noop` sink, any other URL an `http` one
        let noop_url = self
            .url
            .scheme_str()
            .is_some_and(|scheme| scheme.eq_ignore_ascii_case("noop"));
        let sink = if noop_url {
            JobSink::Noop
        } else {
            JobSink::Http(HttpMeta {
                method: self.method.take().unwrap_or(Method::POST),
                url: self.url.clone(),
            })
        };
        errors.check("url", sink.check(targets));

        let mut headers = errors
            .check(
                "headers",
                normalize_headers(self.headers.take().unwrap_or_default(), is_reserved_header),
            )
            .unwrap_or_default();
        let body = match (self.body_raw.take(), &self.body, &self.body_base64) {
            (Some(raw), ..) => raw,
            (None, Some(_), Some(_)) => {
                errors.add("body_base64", "cannot be set together with body");
                Bytes::new()
            }
            (None, None, Some(encoded)) => errors
                .check(
                    "body_base64",
                    base64::engine::general_purpose::STANDARD
                        .decode(encoded.trim())
                        .map_err(|_| "is not valid base64"),
                )
                .map(Bytes::from)
                .unwrap_or_default(),
            (None, None, None) => Bytes::new(),
            (None, Some(json), None) if json.get().starts_with('"') => errors
                .check(
                    "body",
                    serde_json::from_str::<String>(json.get()).map_err(|err| err.to_string()),
                )
                .map(Bytes::from)
                .unwrap_or_default(),
            (None, Some(json), None) => {
                headers
                    .entry("content-type".into())
                    .or_insert_with(|| "application/json".into());
                Bytes::from(json.get().to_owned())
            }
        };

        let callback = self
            .callback
            .take()
            .map(|callback| check_callback(callback, targets, &mut errors));
        if self.at.is_some() && self.delay.is_some() {
            errors.add("delay", "cannot be set together with at");
        }
        let at = self.at.or_else(|| {
            self.delay
                .map(|secs| now + chrono::Duration::seconds(i64::from(secs)))
        });
        let schedule = self.schedule(&mut errors);
        // a schedule runs at least once; a time of the past is now
        if let Some(parameter) = schedule
            .as_ref()
            .and_then(|schedule| schedule.never_runs(at.map_or(now, |at| at.max(now))))
        {
            errors.add(parameter, "the schedule has no run before it ends");
        }
        if self.key.as_deref().is_some_and(|key| !is_key(key)) {
            errors.add("key", KEY_RULE);
        }

        if !errors.0.is_empty() {
            return Err(errors.0);
        }
        let meta = JobMeta {
            sink,
            retry: self.retry,
            timeout: self.timeout.map(NonZeroU32::get),
            flow_key: self.flow_key,
            priority: self.priority,
            traceparent: None,
            tracestate: None,
            callback,
            callback_as: None,
        };
        Ok(JobCreate {
            meta,
            headers: Some(headers),
            body,
            at,
            schedule,
            key: self.key,
        }
        .normalized(options, now))
    }

    /// The schedule of `cron` or `interval`, `tz` and `until`; `None` when the job does not
    /// repeat.
    fn schedule(&mut self, errors: &mut Errors) -> Option<JobSchedule> {
        let rule = match (self.cron.take(), self.interval) {
            (None, None) => {
                // `tz` is the zone of nothing, `until` ends nothing
                if self.tz.is_some() {
                    errors.add("tz", "applies to cron and interval only");
                }
                if self.until.is_some() {
                    errors.add("until", "applies to cron and interval only");
                }
                return None;
            }
            (Some(_), Some(_)) => {
                errors.add("cron", "cannot be set together with interval");
                None
            }
            (Some(cron), None) => ScheduleRule::parse_cron(&cron)
                .map_err(|err| errors.0.push(err))
                .ok(),
            (None, Some(0)) => {
                errors.add("interval", ScheduleRule::INTERVAL_RULE);
                None
            }
            (None, Some(interval)) => Some(ScheduleRule::Interval(interval)),
        };
        Some(JobSchedule {
            rule: rule?,
            tz: self.tz.take().unwrap_or(TimeZone::UTC),
            until: self.until,
        })
    }
}

/// What the types of a [`JobCallback`] do not say, however it was made: the target, the method,
/// the headers. Errors go to `errors` (`callback.<field>`); the callback is normalized.
fn check_callback(
    mut callback: JobCallback,
    targets: &TargetPolicy,
    errors: &mut Errors,
) -> JobCallback {
    let target = JobSink::Http(HttpMeta {
        method: Method::POST,
        url: callback.url.clone(),
    });
    errors.check("callback.url", target.check(targets));
    if callback
        .method
        .as_ref()
        .is_some_and(|method| ![Method::POST, Method::PUT].contains(method))
    {
        errors.add("callback.method", "must be POST or PUT");
    }
    if callback.types.as_ref().is_some_and(Vec::is_empty) {
        errors.add(
            "callback.types",
            "cannot be empty: no callback is no callback.url",
        );
    }
    callback.headers = callback
        .headers
        .take()
        .and_then(|headers| {
            errors.check(
                "callback.headers",
                normalize_headers(headers, is_reserved_callback_header),
            )
        })
        .filter(|headers| !headers.is_empty());
    callback
}

/// The errors of a request: every invalid field is reported, not only the first.
#[derive(Default)]
struct Errors(Vec<ParamError>);

impl Errors {
    fn add(&mut self, parameter: &'static str, detail: impl Into<String>) {
        self.0.push(ParamError::new(parameter, detail));
    }

    /// The value of `result`; `None` with an error of `parameter` when it is an error.
    fn check<T>(
        &mut self,
        parameter: &'static str,
        result: Result<T, impl Into<String>>,
    ) -> Option<T> {
        result.map_err(|detail| self.add(parameter, detail)).ok()
    }
}

fn normalize_headers(
    headers: HashMap<String, String>,
    is_reserved: fn(&str) -> bool,
) -> Result<HashMap<String, String>, &'static str> {
    let mut normalized = HashMap::with_capacity(headers.len());
    for (name, value) in headers {
        let name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| "has an invalid name")?;
        if is_reserved(name.as_str()) {
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
    use crate::models::CallbackType;

    #[test]
    fn json_job_preparation() -> anyhow::Result<()> {
        let options = WorkerOptions::for_tests();
        let targets = TargetPolicy::allow_all();
        let now = Utc::now();
        let prepare = |spec| JobRequest::validate(spec, &options, &targets, now);

        let spec = serde_json::from_str(
            r#"{"method":"POST","url":"https://example.com","delay":2,
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
            r#"{"method":"POST","url":"ftp://example.com",
                "headers":{"Authorization":"a","authorization":"b"},"key":""}"#,
        )?;
        let errors = prepare(spec).expect_err("invalid job");
        assert_eq!(
            errors
                .iter()
                .map(|error| error.parameter.as_ref())
                .collect::<Vec<_>>(),
            ["url", "headers", "key"]
        );
        assert!(
            errors
                .iter()
                .any(|error| error.detail.contains("duplicate"))
        );
        // typed fields are refused by the JSON itself
        for meta in [
            r#"{"url":"noop://x","timeout":0}"#,
            r#"{"url":"noop://x","callback":{"url":"https://example.com","types":["completed"]}}"#,
            // a policy has its kind, a backoff its delay, and a retry is an object
            r#"{"url":"noop://x","retry":{"count":3}}"#,
            r#"{"url":"noop://x","retry":{"policy":"fixd","count":3}}"#,
            r#"{"url":"noop://x","retry":{"policy":"fibonacci","count":3}}"#,
            r#"{"url":"noop://x","retry":"fixed","retry_count":3}"#,
            // a callback has its url
            r#"{"url":"noop://x","callback":{}}"#,
            r#"{"url":"noop://x","callback":{"timeout":5}}"#,
            r#"{"url":"noop://x","callback":{"url":"https://example.com","retry":{"count":0}}}"#,
            r#"{"url":"noop://x","callback":{"url":"https://example.com","timeout":0}}"#,
            // a policy as text is the one of `_retry`
            r#"{"url":"noop://x","retry":"5|exponential"}"#,
            // a flow key, a time
            r#"{"url":"noop://x","flow_key":"Bad Name"}"#,
            r#"{"url":"noop://x","at":"tomorrow"}"#,
            r#"{"url":"noop://x","at":true}"#,
        ] {
            assert!(serde_json::from_str::<JobRequest>(meta).is_err(), "{meta}");
        }

        // callbacks: the retry fields, the method in any case, an empty list of types
        let spec = serde_json::from_str(
            r#"{"url":"noop://x","callback":{"url":"https://example.com/cb","method":"put",
                "retry":{"policy":"fixed","count":10,"delay":30},
                "types":["irisqo.job.failed"]}}"#,
        )?;
        let callback = prepare(spec)
            .expect("callback")
            .meta
            .callback
            .expect("callback");
        assert_eq!(callback.method, Some(Method::PUT));
        assert_eq!(
            callback.retry,
            Some(JobRetry::Fixed {
                count: 10,
                delay: 30
            })
        );
        assert_eq!(callback.types, Some(vec![CallbackType::Failed]));
        for (meta, parameter) in [
            (
                r#"{"url":"noop://x","callback":{"url":"https://example.com","method":"GET"}}"#,
                "callback.method",
            ),
            (
                r#"{"url":"noop://x","callback":{"url":"https://example.com","types":[]}}"#,
                "callback.types",
            ),
            (
                r#"{"url":"noop://x","callback":{"url":"https://example.com","headers":{"Host":"x"}}}"#,
                "callback.headers",
            ),
        ] {
            let errors = prepare(serde_json::from_str(meta)?).expect_err(meta);
            assert_eq!(errors[0].parameter, parameter, "{meta}");
        }

        // no retry, or null: no retries; the text of `_retry` is a policy too
        for meta in [
            r#"{"url":"noop://x"}"#,
            r#"{"url":"noop://x","retry":null}"#,
        ] {
            let spec = serde_json::from_str(meta)?;
            assert_eq!(prepare(spec).expect(meta).meta.retry, None, "{meta}");
        }
        let job = prepare(serde_json::from_str(
            r#"{"url":"noop://x","retry":"5|exponential|2",
                "callback":{"url":"https://example.com/cb","retry":"none"}}"#,
        )?)
        .expect("valid job");
        assert_eq!(
            job.meta.retry,
            Some(JobRetry::Exponential {
                count: 5,
                delay: 2,
                max_delay: None
            })
        );
        assert_eq!(
            job.meta.callback.and_then(|c| c.retry),
            Some(JobRetry::NONE)
        );

        // a method in any case, an empty flow key of the callback, unix 0 (now): as in enqueue_v1
        let job = prepare(serde_json::from_str(
            r#"{"method":"post","url":"https://example.com","at":0,
                "callback":{"url":"https://example.com/cb","flow_key":""}}"#,
        )?)
        .expect("valid job");
        assert!(matches!(&job.meta.sink, JobSink::Http(http) if http.method == Method::POST));
        assert_eq!(job.meta.callback.expect("callback").flow_key, None);
        assert_eq!(job.at, None, "a time of the past is now");

        // a job repeats: the key is the id of the schedule
        let job = prepare(serde_json::from_str(
            r#"{"url":"noop://x","key":"nightly","cron":"0 9 * * *","tz":"Europe/Berlin"}"#,
        )?)
        .expect("a schedule");
        assert_eq!(job.key.as_deref(), Some("nightly"));
        assert_eq!(
            job.schedule.map(|s| s.tz),
            Some(TimeZone::get("Europe/Berlin").unwrap())
        );
        for (meta, parameter) in [
            (r#"{"url":"noop://x","interval":0}"#, "interval"),
            (
                r#"{"url":"noop://x","cron":"* * * * *","interval":60}"#,
                "cron",
            ),
            (
                r#"{"url":"noop://x","until":"2100-01-01T00:00:00Z"}"#,
                "until",
            ),
            (r#"{"url":"noop://x","tz":"Europe/Berlin"}"#, "tz"),
            (r#"{"url":"noop://x","at":2000000000,"delay":5}"#, "delay"),
        ] {
            let errors = prepare(serde_json::from_str(meta)?).expect_err(meta);
            assert_eq!(errors[0].parameter, parameter, "{meta}");
        }

        // the sink comes from the url, the method is POST
        let sink = |meta: &str| -> anyhow::Result<JobSink> {
            Ok(prepare(serde_json::from_str(meta)?).expect(meta).meta.sink)
        };
        assert!(matches!(
            sink(r#"{"url":"https://example.com"}"#)?,
            JobSink::Http(http) if http.method == Method::POST
        ));
        assert_eq!(sink(r#"{"url":"noop://bench"}"#)?, JobSink::Noop);
        assert_eq!(sink(r#"{"url":"NOOP://bench"}"#)?, JobSink::Noop);
        assert_eq!(sink(r#"{"url":"noop://x"}"#)?, JobSink::Noop);
        // the structure: no url, a url or a method that is not one
        for (meta, error) in [
            (r#"{}"#, "missing field `url`"),
            (r#"{"method":"GET"}"#, "missing field `url`"),
            (r#"{"url":"not a url"}"#, "url: is not a valid URL"),
            (
                r#"{"url":"noop://x","callback":{"url":"not a url"}}"#,
                "callback.url: is not a valid URL",
            ),
            (
                r#"{"url":"https://example.com","method":"BAD METHOD"}"#,
                "method: is not a valid HTTP method",
            ),
        ] {
            let err = serde_json::from_str::<JobRequest>(meta).expect_err(meta);
            assert!(err.to_string().contains(error), "{meta}: {err}");
        }
        Ok(())
    }

    #[test]
    fn parameters_fill_the_same_fields() {
        let params = |pairs: &[(&str, &str)]| {
            pairs
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .collect::<HashMap<_, _>>()
        };
        let (input, errors) = JobRequest::http(&Method::GET, HashMap::new(), Bytes::new())
            .with_params(&params(&[
                ("delay", "5"),
                ("retry", "3|fibonacci|2|60"),
                ("key", ""),
            ]));
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!((input.delay, input.key.as_deref()), (Some(5), None));
        assert_eq!(
            input.retry,
            Some(JobRetry::Fibonacci {
                count: 3,
                delay: 2,
                max_delay: Some(60)
            })
        );
        // every one that does not parse, named by its field
        let (input, errors) = JobRequest::http(&Method::GET, HashMap::new(), Bytes::new())
            .with_params(&params(&[
                ("delay", "soon"),
                ("timeout", "0"),
                ("priority", "high"),
                ("flow_key", "q"),
            ]));
        // the others are kept, for `validate` to check them
        assert_eq!(input.flow_key.as_ref().map(FlowKey::as_str), Some("q"));
        let mut names: Vec<_> = errors.iter().map(|e| e.parameter.as_ref()).collect();
        names.sort_unstable();
        assert_eq!(names, ["delay", "priority", "timeout"]);
    }

    /// A schedule as `PUT /api/v1/schedules/nightly` declares it: the id is the key.
    fn declare(json: &str) -> Result<JobCreate, Vec<ParamError>> {
        let mut request = serde_json::from_str::<JobRequest>(json).expect("job");
        request.key = Some("nightly".to_owned());
        request.validate(
            &WorkerOptions::for_tests(),
            &TargetPolicy::allow_all(),
            Utc::now(),
        )
    }

    fn schedule_parameters(errors: &[ParamError]) -> Vec<&str> {
        errors
            .iter()
            .map(|error| error.parameter.as_ref())
            .collect()
    }

    const META: &str = r#""method":"POST","url":"https://example.com""#;

    #[test]
    fn a_schedule_comes_with_the_job_of_its_runs() {
        let job = declare(&format!(
            r#"{{{META},"cron":"*/5 * * * *","until":"2100-01-01T00:00:00Z"}}"#
        ))
        .expect("valid schedule");
        assert!(matches!(
            job.schedule,
            Some(JobSchedule {
                rule: ScheduleRule::Cron(_),
                ..
            })
        ));
        assert_eq!(
            job.schedule.as_ref().and_then(|s| s.until),
            JobSchedule::from_unix_secs(4_102_444_800)
        );
        assert_eq!(
            JobSchedule::parse_time("2100-01-01T00:00:00Z"),
            JobSchedule::parse_time("4102444800"),
            "the same time as RFC 3339 and as unix seconds"
        );
        assert_eq!(job.key.as_deref(), Some("nightly"));

        let job = declare(&format!(r#"{{{META},"interval":300}}"#)).expect("valid schedule");
        assert_eq!(
            job.schedule,
            Some(JobSchedule {
                rule: ScheduleRule::Interval(300),
                tz: TimeZone::UTC,
                until: None
            })
        );
        assert_eq!(
            job.key.as_deref(),
            Some("nightly"),
            "the id of the schedule"
        );
    }

    #[test]
    fn every_invalid_field_is_reported() {
        let errors = declare(
            r#"{"method":"POST","url":"ftp://example.com",
                "cron":"* * * * *","interval":60}"#,
        )
        .expect_err("invalid schedule");
        assert_eq!(schedule_parameters(&errors), ["url", "cron"]);
        // a time or a zone that is not one is refused by the JSON itself
        for field in [r#""until":"soon""#, r#""tz":"Mars/Base""#] {
            let json = format!("{{{META},{field}}}");
            assert!(
                serde_json::from_str::<JobRequest>(&json).is_err(),
                "{field}"
            );
        }
    }

    #[test]
    fn a_schedule_is_cron_or_interval() {
        for (schedule, parameter) in [
            (r#","cron":"nope""#, "cron"),
            (r#","cron":"300""#, "cron"),
            (r#","interval":0"#, "interval"),
        ] {
            let errors = declare(&format!("{{{META}{schedule}}}")).expect_err("invalid schedule");
            assert_eq!(schedule_parameters(&errors), [parameter], "{schedule}");
        }
    }

    #[test]
    fn a_cron_schedule_has_a_time_zone() {
        let job = declare(&format!(
            r#"{{{META},"cron":"0 9 * * *","tz":"Europe/Berlin"}}"#
        ))
        .expect("valid schedule");
        assert_eq!(
            job.schedule.map(|s| s.tz),
            Some(TimeZone::get("Europe/Berlin").unwrap())
        );
        let job = declare(&format!(r#"{{{META},"cron":"0 9 * * *"}}"#)).expect("valid schedule");
        assert_eq!(job.schedule.map(|s| s.tz), Some(TimeZone::UTC));
    }

    #[test]
    fn a_schedule_runs_at_least_once() {
        let errors = declare(&format!(
            r#"{{{META},"interval":3600,"until":"1970-01-01T00:00:01Z"}}"#
        ))
        .expect_err("no run before until");
        assert_eq!(schedule_parameters(&errors), ["until"]);
    }
}
