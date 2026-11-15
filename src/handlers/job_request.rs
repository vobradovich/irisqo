use super::http::{KEY_RULE, TIME_RULE, is_key};
use crate::features::schedules::JobSchedule;
use crate::models::headers::{is_reserved_callback_header, is_reserved_header};
use crate::models::{
    CallbackMode, CallbackType, FlowKey, HttpMeta, JobCallback, JobCreate, JobMeta, JobRetry,
    JobSink, ParamError, WorkerOptions,
};
use axum::body::Bytes;
use base64::Engine as _;
use chrono::{DateTime, Utc};
use hyper::{
    Method, Uri,
    header::{HeaderName, HeaderValue},
};
use serde::Deserialize;
use serde_json::{Value, value::RawValue};
use std::collections::HashMap;
use utoipa::{
    ToSchema,
    openapi::schema::{KnownFormat, ObjectBuilder, OneOfBuilder, SchemaFormat, Type},
};

/// Where a job goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SinkKind {
    /// An HTTP request: `method` and `url`.
    Http,
    /// Completes at once and does nothing: for dry runs and load tests.
    #[serde(alias = "none")]
    Noop,
}

/// JSON job format v1, flat like `enqueue_v1`, the `/to/` query and the "New job" form: the one
/// description of a job that every way to create one fills (JSON by serde, `/to/` and the form
/// by [`JobRequest::set_param`]) and [`JobRequest::validate`] checks.
///
/// `sink` (`http` with `method`, `url`; `noop`), `retry` (`fixed`, `fibonacci`, `exponential`
/// with `retry_count`, `retry_delay`, which `fixed` may leave out to retry at once, and
/// `retry_max_delay` of the last two), `delay` (seconds), `timeout` (milliseconds), `flow_key`,
/// `priority`, `headers`, `body` or `body_base64`, `key`, `at`.
/// The retry policy defaults to none, the timeout to `--timeout` when the job runs. Fields that
/// are not known are ignored.
#[derive(Debug, Default, Deserialize, ToSchema)]
pub struct JobRequest {
    /// Required.
    pub(crate) sink: Option<SinkKind>,
    /// An HTTP method, for the `http` sink.
    #[schema(schema_with = method_schema)]
    pub(crate) method: Option<String>,
    /// An `http` or `https` URL, for the `http` sink.
    pub(crate) url: Option<String>,
    /// The retry policy: `retry` is `fixed` (with `retry_count`, `retry_delay` optional),
    /// `fibonacci` or `exponential` (with `retry_count`, `retry_delay`, `retry_max_delay` optional).
    /// None, or one that does not parse, means no retries: a client generated from the API
    /// description sends what it has.
    #[serde(flatten)]
    #[schema(schema_with = retry_schema)]
    pub(crate) retry: Option<JobRetry>,
    /// Milliseconds; `--timeout` when absent.
    #[schema(minimum = 1)]
    pub(crate) timeout: Option<u32>,
    /// Flow key of the job: 1 to 128 characters of `a-z 0-9 . _ : -`.
    pub(crate) flow_key: Option<String>,
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
    /// When the job runs: an RFC 3339 time (a string) or unix seconds (a number). Wins over `delay`.
    #[schema(schema_with = at_schema)]
    pub(crate) at: Option<Value>,
    /// Seconds from now, when `at` is not set.
    pub(crate) delay: Option<u32>,
    /// A repeated key returns the existing job.
    pub(crate) key: Option<String>,
    /// An `http` or `https` URL: a request that irisqo makes when the job reaches its outcome.
    pub(crate) callback_url: Option<String>,
    /// `POST` (the default) or `PUT`.
    #[schema(schema_with = callback_method_schema)]
    pub(crate) callback_method: Option<String>,
    /// `webhook` (the default): JSON as Standard Webhooks, signed when the server has a key.
    /// `relay`: the response of the job as it is, with `Irisqo-*` headers.
    pub(crate) callback_mode: Option<CallbackMode>,
    /// The types that make a callback: `irisqo.job.completed`, `irisqo.job.failed`,
    /// `irisqo.job.cancelled`. A `webhook`: all three when absent; a `relay`: `completed`.
    pub(crate) callback_types: Option<Vec<String>>,
    /// Headers for the callback, kept like `headers`.
    #[schema(value_type = Option<HashMap<String, String>>)]
    pub(crate) callback_headers: Option<HashMap<String, String>>,
    /// The retry policy of the callback, as `retry`: the default of a callback when absent
    /// (10 retries over about three days).
    pub(crate) callback_retry: Option<String>,
    pub(crate) callback_retry_count: Option<u16>,
    pub(crate) callback_retry_delay: Option<u32>,
    pub(crate) callback_retry_max_delay: Option<u32>,
    /// Milliseconds; 30000 when absent.
    #[schema(minimum = 1)]
    pub(crate) callback_timeout: Option<u32>,
    /// Flow key of the callback: not the one of the job.
    pub(crate) callback_flow_key: Option<String>,
    /// Higher runs first, among the jobs without a flow or in its flow.
    pub(crate) callback_priority: Option<i32>,
    /// The body as it came (`/to/`, the form): it is sent as it is, `body` is not looked at.
    #[serde(skip)]
    pub(crate) body_raw: Option<Bytes>,
    /// The retry policy of the callback as `/to/` gives it (`_callback_retry`).
    #[serde(skip)]
    pub(crate) callback_retry_policy: Option<JobRetry>,
}

/// A string of one of `values`, for the API description.
fn one_of(values: &[&str]) -> utoipa::openapi::Object {
    ObjectBuilder::new()
        .schema_type(Type::String)
        .enum_values(Some(values.iter().copied()))
        .build()
}

fn callback_method_schema() -> utoipa::openapi::Object {
    one_of(&["POST", "PUT"])
}

fn method_schema() -> utoipa::openapi::Object {
    one_of(&["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"])
}

/// An integer from `min` to `max`, for the API description.
fn integer(min: i64, max: i64) -> utoipa::openapi::Object {
    ObjectBuilder::new()
        .schema_type(Type::Integer)
        .minimum(Some(min))
        .maximum(Some(max))
        .build()
}

/// The fields of [`JobRetry`], all optional: a job without a policy has none of them.
pub(crate) fn retry_schema() -> utoipa::openapi::Object {
    ObjectBuilder::new()
        .property(
            "retry",
            one_of(&["none", "fixed", "fibonacci", "exponential"]),
        )
        .property("retry_count", integer(0, u16::MAX.into()))
        .property("retry_delay", integer(0, u32::MAX.into()))
        .property("retry_max_delay", integer(1, u32::MAX.into()))
        .build()
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

fn parse_at(at: &Value) -> Option<DateTime<Utc>> {
    match at {
        Value::Number(n) => n
            .as_i64()
            .and_then(|secs| DateTime::from_timestamp(secs, 0)),
        Value::String(s) => JobSchedule::parse_time(s),
        Value::Null | Value::Bool(_) | Value::Array(_) | Value::Object(_) => None,
    }
}

/// For `map_err`: a parameter whose value does not parse.
fn refuse<E>(parameter: &'static str, rule: &'static str) -> impl FnOnce(E) -> ParamError {
    move |_| ParamError::new(parameter, rule)
}

impl JobRequest {
    /// An `http` job for `/to/` and the form: the target, the headers to send, the body as it is.
    pub(crate) fn http(
        method: &Method,
        url: &str,
        headers: HashMap<String, String>,
        body: Bytes,
    ) -> Self {
        Self {
            sink: Some(SinkKind::Http),
            method: Some(method.to_string()),
            url: Some(url.to_owned()),
            headers: Some(headers),
            body_raw: Some(body),
            ..Self::default()
        }
    }

    /// Sets a job parameter of `/to/` (`_delay`, `_flow_key`...); `Ok(false)` when `name` is not one.
    pub(crate) fn set_param(&mut self, name: &str, value: &str) -> Result<bool, ParamError> {
        match name {
            "_delay" => {
                self.delay = Some(
                    value
                        .parse()
                        .map_err(refuse("_delay", "must be a number of seconds"))?,
                );
            }
            "_at" => self.at = Some(Value::String(value.to_owned())),
            "_timeout" => {
                self.timeout = Some(value.parse().map_err(refuse(
                    "_timeout",
                    "must be a positive number of milliseconds",
                ))?);
            }
            "_retry" => {
                self.retry = JobRetry::parse(value).map_err(refuse("_retry", JobRetry::RULE))?;
            }
            "_flow_key" => self.flow_key = Some(value.to_owned()),
            "_priority" => {
                self.priority = Some(value.parse().map_err(refuse(
                    "_priority",
                    "must be an integer from -2147483648 to 2147483647",
                ))?);
            }
            "_key" => self.key = (!value.is_empty()).then(|| value.to_owned()),
            "_callback_url" => self.callback_url = (!value.is_empty()).then(|| value.to_owned()),
            "_callback_mode" => {
                self.callback_mode = Some(match value {
                    "webhook" => CallbackMode::Webhook,
                    "relay" => CallbackMode::Relay,
                    _ => {
                        return Err(ParamError::new(
                            "_callback_mode",
                            "must be webhook or relay",
                        ));
                    }
                });
            }
            "_callback_method" => self.callback_method = Some(value.to_owned()),
            "_callback_types" => {
                self.callback_types = Some(value.split(',').map(|t| t.trim().to_owned()).collect());
            }
            "_callback_retry" => {
                self.callback_retry_policy = Some(
                    JobRetry::parse(value)
                        .map_err(refuse("_callback_retry", JobRetry::RULE))?
                        .unwrap_or(JobRetry::NONE),
                );
            }
            "_callback_timeout" => {
                self.callback_timeout = Some(value.parse().map_err(refuse(
                    "_callback_timeout",
                    "must be a positive number of milliseconds",
                ))?);
            }
            "_callback_flow_key" => self.callback_flow_key = Some(value.to_owned()),
            "_callback_priority" => {
                self.callback_priority = Some(value.parse().map_err(refuse(
                    "_callback_priority",
                    "must be an integer from -2147483648 to 2147483647",
                ))?);
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    /// The retry policy of the callback: from `/to/`, or from the `callback_retry*` fields.
    /// `None` is a policy that is not given (the default of a callback); `none` is
    /// [`JobRetry::NONE`]. A policy that is not valid is an error.
    fn callback_retry(&self) -> Result<Option<JobRetry>, String> {
        if self.callback_retry_policy.is_some() {
            return Ok(self.callback_retry_policy);
        }
        let Some(policy) = self.callback_retry.as_deref() else {
            return Ok(None);
        };
        if matches!(policy, "" | "none") {
            return Ok(Some(JobRetry::NONE));
        }
        let mut fields = serde_json::Map::new();
        fields.insert("retry".into(), policy.into());
        for (name, value) in [
            ("retry_count", self.callback_retry_count.map(u32::from)),
            ("retry_delay", self.callback_retry_delay),
            ("retry_max_delay", self.callback_retry_max_delay),
        ] {
            if let Some(value) = value {
                fields.insert(name.into(), value.into());
            }
        }
        serde_json::from_value(Value::Object(fields))
            .map(Some)
            .map_err(|err| err.to_string())
    }

    /// Validates the description of a job: every invalid field is reported, not only the first.
    /// The `parameter` of an error is the name of the field.
    pub(crate) fn validate(
        mut self,
        options: &WorkerOptions,
        now: DateTime<Utc>,
    ) -> Result<JobCreate, Vec<ParamError>> {
        let mut errors = Vec::new();
        let mut bad = |parameter, detail: &str| errors.push(ParamError::new(parameter, detail));

        let sink = match self.sink {
            None => {
                bad("sink", "is required");
                None
            }
            Some(SinkKind::Noop) => Some(JobSink::Noop),
            Some(SinkKind::Http) => {
                let method = match self.method.as_deref() {
                    None => {
                        bad("method", "is required");
                        None
                    }
                    Some(method) => method
                        .parse::<Method>()
                        .map_err(|_| bad("method", "is not a valid HTTP method"))
                        .ok(),
                };
                let url = match self.url.as_deref() {
                    None => {
                        bad("url", "is required");
                        None
                    }
                    Some(url) => Uri::try_from(url)
                        .map_err(|_| bad("url", "is not a valid URL"))
                        .ok(),
                };
                method
                    .zip(url)
                    .map(|(method, url)| JobSink::Http(HttpMeta { method, url }))
            }
        };
        if let Some(sink) = &sink
            && let Err(detail) = sink.check(options)
        {
            bad("url", detail);
        }
        // none: the server's default when the job runs
        if self.timeout == Some(0) {
            bad("timeout", "must be a positive number of milliseconds");
        }
        // an empty flow is none, as in `enqueue_v1`
        let flow_key = self
            .flow_key
            .take()
            .filter(|key| !key.is_empty())
            .and_then(|key| {
                FlowKey::try_from(key)
                    .map_err(|rule| bad("flow_key", rule))
                    .ok()
            });

        let mut headers =
            normalize_headers(self.headers.take().unwrap_or_default(), is_reserved_header)
                .map_err(|detail| bad("headers", detail))
                .unwrap_or_default();
        let body = match (&self.body_raw, &self.body, &self.body_base64) {
            (Some(raw), ..) => raw.clone(),
            (None, Some(_), Some(_)) => {
                bad("body_base64", "cannot be set together with body");
                Bytes::new()
            }
            (None, None, Some(encoded)) => base64::engine::general_purpose::STANDARD
                .decode(encoded.trim())
                .map(Bytes::from)
                .map_err(|_| bad("body_base64", "is not valid base64"))
                .unwrap_or_default(),
            (None, None, None) => Bytes::new(),
            (None, Some(json), None) if json.get().starts_with('"') => {
                serde_json::from_str::<String>(json.get())
                    .map(Bytes::from)
                    .map_err(|err| bad("body", &err.to_string()))
                    .unwrap_or_default()
            }
            (None, Some(json), None) => {
                headers
                    .entry("content-type".into())
                    .or_insert_with(|| "application/json".into());
                Bytes::from(json.get().to_owned())
            }
        };
        if self.key.as_deref().is_some_and(|key| !is_key(key)) {
            bad("key", KEY_RULE);
        }
        let at = self.at.as_ref().map(|at| {
            parse_at(at).unwrap_or_else(|| {
                bad("at", TIME_RULE);
                now
            })
        });
        let callback_retry = self.callback_retry().unwrap_or_else(|detail| {
            bad("callback_retry", &detail);
            None
        });
        let types_given = self.callback_types.is_some();
        let callback_method = self.callback_method.take().and_then(|method| {
            match method.to_ascii_uppercase().as_str() {
                "POST" => Some(Method::POST),
                "PUT" => Some(Method::PUT),
                _ => {
                    bad("callback_method", "must be POST or PUT");
                    None
                }
            }
        });
        let callback_types = self.callback_types.take().and_then(|list| {
            if list.is_empty() {
                bad(
                    "callback_types",
                    "cannot be empty: no callback is no callback_url",
                );
                return None;
            }
            let types: Option<Vec<_>> = list.iter().map(|t| CallbackType::parse(t)).collect();
            if types.is_none() {
                bad("callback_types", CallbackType::RULE);
            }
            types
        });
        let callback = match self.callback_url.take() {
            None => {
                if self.callback_mode.is_some()
                    || types_given
                    || callback_method.is_some()
                    || self.callback_headers.is_some()
                    || callback_retry.is_some()
                    || self.callback_retry.is_some()
                    || self.callback_timeout.is_some()
                    || self.callback_flow_key.is_some()
                    || self.callback_priority.is_some()
                {
                    bad("callback_url", "is required by the other callback fields");
                }
                None
            }
            Some(url) => {
                let url = Uri::try_from(url.as_str())
                    .map_err(|_| bad("callback_url", "is not a valid URL"))
                    .ok();
                let url = url.filter(|url| {
                    let sink = JobSink::Http(HttpMeta {
                        method: Method::POST,
                        url: url.clone(),
                    });
                    sink.check(options)
                        .map_err(|detail| bad("callback_url", detail))
                        .is_ok()
                });
                if self.callback_timeout == Some(0) {
                    bad(
                        "callback_timeout",
                        "must be a positive number of milliseconds",
                    );
                }
                let flow_key = self
                    .callback_flow_key
                    .take()
                    .filter(|key| !key.is_empty())
                    .and_then(|key| {
                        FlowKey::try_from(key)
                            .map_err(|rule| bad("callback_flow_key", rule))
                            .ok()
                    });
                let headers = self.callback_headers.take().map(|headers| {
                    normalize_headers(headers, is_reserved_callback_header)
                        .map_err(|detail| bad("callback_headers", detail))
                        .unwrap_or_default()
                });
                url.map(|url| JobCallback {
                    url,
                    method: callback_method,
                    mode: self.callback_mode.unwrap_or_default(),
                    types: callback_types,
                    headers: headers.filter(|headers| !headers.is_empty()),
                    retry: callback_retry,
                    timeout: self.callback_timeout,
                    flow_key,
                    priority: self.callback_priority,
                })
            }
        };

        let Some(sink) = sink.filter(|_| errors.is_empty()) else {
            return Err(errors);
        };
        // `at` wins over `delay`
        let at = at.or_else(|| {
            self.delay
                .map(|secs| now + chrono::Duration::seconds(i64::from(secs)))
        });
        let meta = JobMeta {
            sink,
            retry: self.retry,
            timeout: self.timeout,
            flow_key,
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
            schedule: None,
            key: self.key,
        }
        .normalized(options, now))
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

    #[test]
    fn json_job_preparation() -> anyhow::Result<()> {
        let options = WorkerOptions::for_tests();
        let now = Utc::now();
        let prepare = |spec| JobRequest::validate(spec, &options, now);

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
            ["url", "timeout", "headers", "key"]
        );
        assert!(
            errors
                .iter()
                .any(|error| error.detail.contains("duplicate"))
        );

        // a retry that is not a policy means no retries, not an error (see `JobRequest::retry`)
        for meta in [
            r#"{"sink":"noop","retry":"fixd"}"#,
            r#"{"sink":"noop","retry":"fibonacci","retry_count":3}"#,
            r#"{"sink":"noop","retry":"none"}"#,
        ] {
            let spec = serde_json::from_str(meta)?;
            assert_eq!(prepare(spec).expect(meta).meta.retry, None, "{meta}");
        }

        // the structure: no sink, no url, no method, a bad method
        for (meta, parameter) in [
            (r#"{"url":"https://example.com"}"#, "sink"),
            (r#"{"sink":"http"}"#, "method"),
            (r#"{"sink":"http","url":"https://example.com"}"#, "method"),
            (
                r#"{"sink":"http","url":"https://example.com","method":"BAD METHOD"}"#,
                "method",
            ),
            (r#"{"sink":"http","method":"GET"}"#, "url"),
        ] {
            let spec = serde_json::from_str(meta)?;
            let errors = prepare(spec).expect_err(meta);
            assert_eq!(errors[0].parameter, parameter, "{meta}");
        }
        Ok(())
    }

    #[test]
    fn parameters_fill_the_same_fields() {
        let mut input = JobRequest::default();
        for (name, value) in [
            ("_delay", "5"),
            ("_retry", "3|fibonacci|2|60"),
            ("_key", ""),
        ] {
            assert!(input.set_param(name, value).expect(name));
        }
        assert_eq!((input.delay, input.key.as_deref()), (Some(5), None));
        assert_eq!(
            input.retry,
            Some(JobRetry::Fibonacci {
                retry_count: 3,
                retry_delay: 2,
                retry_max_delay: Some(60)
            })
        );
        assert!(!input.set_param("a", "b").expect("not ours"));
        assert!(input.set_param("_delay", "soon").is_err());
    }
}
