use std::collections::HashMap;

use bytes::Bytes;
use chrono::{DateTime, Utc};
use http_body_util::Full;
use hyper::{HeaderMap, Method, Uri};

use serde::{Deserialize, Serialize};

use crate::features::schedules::JobSchedule;

use super::{CallbackMode, Error, JobCallback, JobRetry, TargetPolicy, WorkerOptions};

#[derive(Debug, Clone, sqlx::FromRow, Serialize, utoipa::ToSchema)]
pub struct JobRow {
    pub id: i64,
    #[sqlx(json)]
    #[serde(serialize_with = "serialize_meta_shown")]
    pub meta: JobMeta,
    #[sqlx(json(nullable))]
    #[serde(serialize_with = "super::headers::serialize_shown")]
    pub headers: Option<HashMap<String, String>>,
    /// The body as it is stored: a list of bytes.
    pub body: Option<Vec<u8>>,
    pub schedule_id: Option<String>,
    pub key: Option<String>,
    /// The job that made this one: a job with a parent is a callback.
    #[serde(skip_serializing_if = "Option::is_none")]
    // not in what the claim returns: the runner does not need it
    #[sqlx(default)]
    pub parent_id: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, sqlx::FromRow)]
pub struct JobEntry {
    pub id: i64,
    pub attempt: i32,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct JobWithAttempt {
    #[sqlx(flatten)]
    pub job: JobRow,
    pub attempt: i32,
}

#[derive(Debug, Clone, Default)]
pub struct JobCreate {
    pub meta: JobMeta,
    pub headers: Option<HashMap<String, String>>,
    pub body: Bytes,
    pub at: Option<DateTime<Utc>>,
    pub schedule: Option<JobSchedule>,
    pub key: Option<String>,
}

impl JobCreate {
    /// The rules every way to create a job shares: the timeout is capped by `--max-timeout`, a
    /// time that has passed means now, the trace context is the one of this request unless the
    /// job brings its own.
    #[must_use]
    pub fn normalized(mut self, options: &WorkerOptions, now: DateTime<Utc>) -> Self {
        self.meta.timeout = self.meta.timeout.map(|ms| ms.min(options.max_timeout));
        if let Some(callback) = self.meta.callback.as_mut() {
            callback.timeout = callback
                .timeout
                .and_then(|ms| std::num::NonZeroU32::new(ms.get().min(options.max_timeout)));
        }
        self.at = self.at.filter(|at| *at > now);
        if self.meta.traceparent.is_none() {
            (self.meta.traceparent, self.meta.tracestate) = crate::otel::current_trace_headers();
        }
        self
    }

    /// The body as it is stored: none when it is empty.
    pub fn body_bytes(&self) -> Option<&[u8]> {
        (!self.body.is_empty()).then_some(self.body.as_ref())
    }
}

#[derive(Debug, sqlx::FromRow, Serialize, utoipa::ToSchema)]
pub struct JobCreateRow {
    pub id: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schedule_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default, utoipa::ToSchema)]
pub struct JobMeta {
    #[serde(flatten, default)]
    pub sink: JobSink,
    /// No retries when absent.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub retry: Option<JobRetry>,
    /// Milliseconds; the API stores `--timeout` (default 3000) when the request has none.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub timeout: Option<u32>,
    /// Flow key of the job.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub flow_key: Option<FlowKey>,
    /// Higher runs first within its flow, or among the jobs without a flow (0 when absent);
    /// the flows and the jobs without a flow take turns, the priority does not rank across them.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub priority: Option<i32>,
    /// W3C trace context of the request that created the job: the parent of every
    /// attempt (the link of every schedule run), copied to every run of a schedule.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub traceparent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub tracestate: Option<String>,
    /// The request made when the job reaches its outcome.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub callback: Option<JobCallback>,
    /// Set on a callback job only, by the callback that made it (no request can set it): the
    /// mode it is sent in, which says how its delivery is finished and signed.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub callback_as: Option<CallbackMode>,
}

/// Serializes the meta of a job as the API shows it: the values of the callback headers meant
/// for the receiver are hidden, as the headers of the job are. The stored meta is not this one.
fn serialize_meta_shown<S: serde::Serializer>(
    meta: &JobMeta,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    // only a callback with headers needs a copy to hide them in
    if meta.callback.as_ref().is_none_or(|c| c.headers.is_none()) {
        return meta.serialize(serializer);
    }
    let mut shown = meta.clone();
    if let Some(callback) = shown.callback.as_mut() {
        callback.headers = callback.headers.take().map(|headers| {
            headers
                .into_iter()
                .map(|(name, value)| {
                    let value = super::headers::shown_value(&name, &value).to_owned();
                    (name, value)
                })
                .collect()
        });
    }
    shown.serialize(serializer)
}

/// An HTTP method in upper case: `post` is `POST`, not a method of its own (as in `enqueue_v1`).
pub fn parse_method(method: &str) -> Option<Method> {
    Method::from_bytes(method.to_ascii_uppercase().as_bytes()).ok()
}

/// An optional HTTP method as [`parse_method`], for `deserialize_with`; the error names `field`.
pub fn deserialize_method_of<'de, D: serde::Deserializer<'de>>(
    field: &str,
    deserializer: D,
) -> Result<Option<Method>, D::Error> {
    Option::<String>::deserialize(deserializer)?
        .map(|method| {
            parse_method(&method).ok_or_else(|| {
                serde::de::Error::custom(format!("{field}: is not a valid HTTP method"))
            })
        })
        .transpose()
}

/// A URL, for `deserialize_with`; the error names `field`.
pub fn deserialize_uri_of<'de, D: serde::Deserializer<'de>>(
    field: &str,
    deserializer: D,
) -> Result<Uri, D::Error> {
    let url = String::deserialize(deserializer)?;
    Uri::try_from(url).map_err(|_| serde::de::Error::custom(format!("{field}: is not a valid URL")))
}

/// The key of a flow, as `enqueue_v1` checks it: 1 to 128 characters of `a-z 0-9 . _ : -`, the
/// first a letter or a digit. Only [`FlowKey::try_from`] (and the deserialization, which uses it)
/// makes one, so a `FlowKey` is always valid. It has no `sqlx::Type` on purpose: the derive would
/// add `Decode`, which builds one from a database value without the check. Bind [`FlowKey::as_str`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(try_from = "String")]
pub struct FlowKey(String);

impl FlowKey {
    /// What is wrong with a name that [`FlowKey::try_from`] refuses.
    pub const RULE: &str =
        "must be 1 to 128 characters of a-z, 0-9, . _ : - and start with a letter or a digit";

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The flow key of a request: an empty one is none, as in `enqueue_v1`.
    pub fn parse_optional(value: &str) -> Result<Option<Self>, &'static str> {
        (!value.is_empty())
            .then(|| Self::try_from(value.to_owned()))
            .transpose()
    }

    /// For `deserialize_with`: a flow key of a request, where an empty one is none.
    pub fn deserialize_optional<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<Self>, D::Error> {
        Option::<String>::deserialize(deserializer)?
            .map_or(Ok(None), |key| Self::parse_optional(&key))
            .map_err(serde::de::Error::custom)
    }
}

impl TryFrom<String> for FlowKey {
    type Error = &'static str;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        let mut chars = value.chars();
        let valid = chars
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
            && value.len() <= 128
            && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || ".:_-".contains(c));
        valid.then_some(FlowKey(value)).ok_or(Self::RULE)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default, utoipa::ToSchema)]
#[serde(tag = "sink")]
#[serde(rename_all = "snake_case")]
pub enum JobSink {
    /// Completes at once and does nothing: for dry runs and load tests.
    #[default]
    #[serde(alias = "none")]
    Noop,
    Http(HttpMeta),
    // Kafka,
    // Redis,
    // RabbitMq,
}

impl JobSink {
    /// Why the target of the job is not allowed, `Ok` when it is. Every sink says it here, so a
    /// new one cannot be added without it (the API paths call this, none checks a sink itself).
    pub fn check(&self, targets: &TargetPolicy) -> Result<(), &'static str> {
        match self {
            JobSink::Noop => Ok(()),
            // a private IP address or localhost; a name is checked when it is resolved
            JobSink::Http(http) => match (http.url.scheme_str(), http.url.host()) {
                (Some("http" | "https"), Some(host)) => targets.check_host(host),
                _ => Err("must be an http or https URL"),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct HttpMeta {
    #[serde(with = "http_serde::method")]
    #[schema(value_type = String, example = "POST")]
    pub method: hyper::Method,
    #[serde(with = "http_serde::uri")]
    #[schema(value_type = String)]
    pub url: hyper::Uri,
}

impl JobRow {
    /// The request of an HTTP job and its body. The body is taken out of the row (it is empty
    /// there after): the one buffer is sent and, for a callback, signed, without a copy.
    pub fn take_request(&mut self) -> Result<(hyper::Request<Full<Bytes>>, Bytes), Error> {
        let JobSink::Http(http) = &self.meta.sink else {
            return Err(Error::invalid("url", "is not an HTTP URL"));
        };
        let mut req = hyper::Request::builder()
            .method(http.method.clone())
            .uri(http.url.clone());
        // `headers_mut()` is `None` only when the builder already failed: `body()` reports it
        if let Some(headers) = &self.headers
            && let Some(headers_mut) = req.headers_mut()
        {
            *headers_mut = HeaderMap::try_from(headers)?;
        }
        let body = self.body.take().map(Bytes::from).unwrap_or_default();
        Ok((req.body(Full::new(body.clone()))?, body))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyper::{Method, Uri, header};

    #[test]
    fn a_sink_checks_its_target() {
        let targets = TargetPolicy::allow_all();
        let http = |url: &str| {
            JobSink::Http(HttpMeta {
                method: Method::GET,
                url: Uri::try_from(url).unwrap(),
            })
        };
        assert_eq!(JobSink::Noop.check(&targets), Ok(()));
        assert_eq!(http("https://example.com").check(&targets), Ok(()));
        for url in ["ftp://example.com", "mailto:a@example.com", "/relative"] {
            assert!(http(url).check(&targets).is_err(), "{url}");
        }
    }

    #[tokio::test]
    async fn job_row_into_request_err() -> anyhow::Result<()> {
        // arrange
        let mut job = JobRow {
            id: 0,
            meta: JobMeta::default(),
            headers: None,
            body: None,
            schedule_id: None,
            key: None,
            parent_id: None,
        };
        // act
        let req = job.take_request();

        // assert
        assert!(req.is_err());

        Ok(())
    }

    #[tokio::test]
    async fn job_row_into_request_ok() -> anyhow::Result<()> {
        // arrange
        let mut job = JobRow {
            id: 0,
            meta: JobMeta {
                sink: JobSink::Http(HttpMeta {
                    method: Method::GET,
                    url: Uri::try_from("http://localhost").unwrap(),
                }),
                retry: Some(JobRetry::Fixed { count: 3, delay: 1 }),
                timeout: Some(2000),
                ..JobMeta::default()
            },
            headers: Some(HashMap::from([(
                header::CONTENT_LENGTH.to_string(),
                "123".into(),
            )])),
            body: Some(b"payload".to_vec()),
            schedule_id: None,
            key: None,
            parent_id: None,
        };
        // act
        let req = job.take_request();

        // assert
        let (req, body) = req.expect("request");
        assert_eq!(req.headers()[header::CONTENT_LENGTH], "123");
        assert_eq!(body.as_ref(), b"payload");
        assert!(job.body.is_none(), "the body is taken out of the row");

        Ok(())
    }
}
