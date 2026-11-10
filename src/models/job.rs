use std::collections::HashMap;

use bytes::Bytes;
use chrono::{DateTime, Utc};
use http_body_util::Full;
use hyper::HeaderMap;

use serde::{Deserialize, Serialize};

use crate::features::schedules::JobSchedule;

use super::{Error, JobRetry, WorkerOptions};

#[derive(Debug, Clone, sqlx::FromRow, Serialize)]
pub struct JobRow {
    pub id: i64,
    #[sqlx(json)]
    pub meta: JobMeta,
    #[sqlx(json(nullable))]
    #[serde(serialize_with = "super::headers::serialize_shown")]
    pub headers: Option<HashMap<String, String>>,
    pub body: Option<Vec<u8>>,
    pub schedule_id: Option<String>,
    pub key: Option<String>,
}

#[derive(Debug, Clone, Copy, sqlx::FromRow)]
pub struct JobEntry {
    pub id: i64,
    pub retry: i32,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct JobWithRetry {
    #[sqlx(flatten)]
    pub job: JobRow,
    pub retry: i32,
}

#[derive(Debug, Clone, Default)]
pub struct JobCreate {
    pub meta: JobMeta,
    pub headers: Option<HashMap<String, String>>,
    pub body: Bytes,
    pub at: Option<DateTime<Utc>>,
    pub schedule: Option<JobSchedule>,
    pub until: Option<DateTime<Utc>>,
    pub key: Option<String>,
}

impl JobCreate {
    /// The body as it is stored: none when it is empty.
    pub fn body_bytes(&self) -> Option<&[u8]> {
        (!self.body.is_empty()).then_some(self.body.as_ref())
    }
}

#[derive(Debug, sqlx::FromRow, Serialize)]
pub struct JobCreateRow {
    pub id: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schedule_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct JobMeta {
    #[serde(flatten, default)]
    pub sink: JobSink,
    #[serde(flatten, default)]
    pub retry: JobRetry,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub delay: Option<u32>,
    /// Milliseconds; the API stores `--timeout` (default 3000) when the request has none.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub timeout: Option<u32>,
    /// W3C trace context of the request that created the job: the parent of every
    /// attempt (the link of every schedule run), copied to every run of a schedule.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub traceparent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub tracestate: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
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
    pub fn check(&self, options: &WorkerOptions) -> Result<(), &'static str> {
        match self {
            JobSink::Noop => Ok(()),
            // a private IP address or localhost; a name is checked when it is resolved
            JobSink::Http(http) => match (http.url.scheme_str(), http.url.host()) {
                (Some("http" | "https"), Some(host)) => options.targets.check_host(host),
                _ => Err("must be an http or https URL"),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HttpMeta {
    #[serde(with = "http_serde::method")]
    pub method: hyper::Method,
    #[serde(with = "http_serde::uri")]
    pub url: hyper::Uri,
}

impl TryFrom<JobRow> for hyper::Request<Full<Bytes>> {
    type Error = Error;

    fn try_from(value: JobRow) -> Result<Self, Self::Error> {
        let meta = value.meta;
        let JobSink::Http(meta) = meta.sink else {
            return Err(Error::invalid("url", "is not an HTTP URL"));
        };

        let body: Bytes = value.body.map_or(Bytes::new(), Bytes::from);
        let mut req = hyper::Request::builder().method(meta.method).uri(meta.url);
        // `headers_mut()` is `None` only when the builder already failed: `body()` reports it
        if let Some(headers) = value.headers
            && let Some(headers_mut) = req.headers_mut()
        {
            for (key, value) in HeaderMap::try_from(&headers)? {
                if let Some(key) = key {
                    headers_mut.insert(key, value);
                }
            }
        };
        Ok(req.body(Full::from(body))?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyper::{Method, Uri, header};

    #[test]
    fn a_sink_checks_its_target() {
        let options = WorkerOptions::for_tests();
        let http = |url: &str| {
            JobSink::Http(HttpMeta {
                method: Method::GET,
                url: Uri::try_from(url).unwrap(),
            })
        };
        assert_eq!(JobSink::Noop.check(&options), Ok(()));
        assert_eq!(http("https://example.com").check(&options), Ok(()));
        for url in ["ftp://example.com", "mailto:a@example.com", "/relative"] {
            assert!(http(url).check(&options).is_err(), "{url}");
        }
    }

    #[tokio::test]
    async fn job_row_into_request_err() -> anyhow::Result<()> {
        // arrange
        let job = JobRow {
            id: 0,
            meta: JobMeta::default(),
            headers: None,
            body: None,
            schedule_id: None,
            key: None,
        };
        // act
        let req = hyper::Request::<Full<Bytes>>::try_from(job);

        // assert
        assert!(req.is_err());

        Ok(())
    }

    #[tokio::test]
    async fn job_row_into_request_ok() -> anyhow::Result<()> {
        // arrange
        let job = JobRow {
            id: 0,
            meta: JobMeta {
                sink: JobSink::Http(HttpMeta {
                    method: Method::GET,
                    url: Uri::try_from("http://localhost").unwrap(),
                }),
                retry: JobRetry::Fixed {
                    retry_count: 3,
                    retry_delay: 1,
                },
                delay: Some(300),
                timeout: Some(2000),
                ..JobMeta::default()
            },
            headers: Some(HashMap::from([(
                header::CONTENT_LENGTH.to_string(),
                "123".into(),
            )])),
            body: None,
            schedule_id: None,
            key: None,
        };
        // act
        let req = hyper::Request::<Full<Bytes>>::try_from(job);

        // assert
        assert!(req.is_ok());

        Ok(())
    }
}
