use std::collections::HashMap;

use bytes::Bytes;
use http_body_util::Full;
use hyper::HeaderMap;
#[cfg(test)]
use hyper::{Method, Uri, header};

use serde::{Deserialize, Serialize};

use crate::features::schedules::JobSchedule;

use super::{Error, JobRetry};

#[derive(Debug, Clone, sqlx::FromRow, Serialize)]
pub struct JobRow {
    pub id: i64,
    #[sqlx(json)]
    pub meta: JobMeta,
    #[sqlx(json(nullable))]
    pub headers: Option<HashMap<String, String>>,
    pub body: Option<Vec<u8>>,
    pub schedule_id: Option<String>,
    pub external_id: Option<String>,
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
    pub at: Option<i64>,
    pub schedule: Option<JobSchedule>,
    pub until: Option<i64>,
    pub external_id: Option<String>,
}

#[derive(Debug, sqlx::FromRow)]
pub struct JobCreateRow {
    pub id: i64,
    pub schedule_id: Option<String>,
    pub external_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct JobMeta {
    #[serde(flatten, default)]
    pub protocol: JobProtocol,
    #[serde(flatten, default)]
    pub retry: JobRetry,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub delay: Option<u32>,
    pub timeout: u32,
    /// W3C trace context of the request that created the job: the parent of every
    /// attempt (the link of every schedule run), copied to every run of a schedule.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub traceparent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub tracestate: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(tag = "protocol")]
#[serde(rename_all = "snake_case")]
pub enum JobProtocol {
    #[default]
    None,
    Http(HttpMeta),
    // Kafka,
    // Redis,
    // RabbitMq,
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
        let JobProtocol::Http(meta) = meta.protocol else {
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

#[tokio::test]
async fn job_row_into_request_err() -> anyhow::Result<()> {
    // arrange
    let job = JobRow {
        id: 0,
        meta: JobMeta::default(),
        headers: None,
        body: None,
        schedule_id: None,
        external_id: None,
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
            protocol: JobProtocol::Http(HttpMeta {
                method: Method::GET,
                url: Uri::try_from("http://localhost").unwrap(),
            }),
            retry: JobRetry::Fixed {
                retry_count: 3,
                retry_delay: 1,
            },
            delay: Some(300),
            timeout: 2000,
            ..JobMeta::default()
        },
        headers: Some(HashMap::from([(
            header::CONTENT_LENGTH.to_string(),
            "123".into(),
        )])),
        body: None,
        schedule_id: None,
        external_id: None,
    };
    // act
    let req = hyper::Request::<Full<Bytes>>::try_from(job);

    // assert
    assert!(req.is_ok());

    Ok(())
}
