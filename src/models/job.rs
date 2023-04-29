use std::{collections::HashMap, time::SystemTime};

use hyper::{header, HeaderMap, Method, Uri};
use serde::{Deserialize, Serialize};
use sqlx::types::Json;

use super::Error;

#[derive(Debug, Clone, sqlx::FromRow, Serialize)]
pub struct JobRow {
    pub id: i64,
    pub protocol: String,
    pub url: String,
    // pub meta: HashMap<String, String>,
    pub headers: Option<Json<HashMap<String, String>>>,
    pub body: Option<Vec<u8>>,
}

#[derive(Debug, sqlx::FromRow)]
pub struct JobQueueRow {
    pub id: i64,
    pub retry: i32,
}

#[derive(Debug, Clone, Default)]
pub struct JobCreate {
    pub meta: JobMeta,
    pub headers: Option<HashMap<String, String>>,
    pub body: Option<Vec<u8>>,
    pub at: Option<SystemTime>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct JobMeta {
    #[serde(flatten)]
    pub protocol: JobProtocol,
    #[serde(flatten)]
    pub retry: JobRetryBackoff,
    pub delay: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(tag = "protocol")]
#[serde(rename_all = "snake_case")]
pub enum JobProtocol {
    #[default]
    Null,
    Http(HttpMeta),
    // Kafka,
    // Redis,
    // RabbitMq,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(tag = "retry_backoff")]
#[serde(rename_all = "snake_case")]
pub enum JobRetryBackoff {
    #[default]
    None,
    Immediate {
        retry: u32,
    },
    Fixed {
        retry: u32,
        retry_delay: u32,
    },
    Fibonacci {
        retry: u32,
        retry_delay: u32,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HttpMeta {
    #[serde(with = "http_serde::method")]
    pub method: hyper::Method,
    #[serde(with = "http_serde::uri")]
    pub url: hyper::Uri,
}

#[derive(Debug, Clone, sqlx::FromRow, Serialize)]
pub struct JobEntry {
    pub id: i64,
    pub meta: JobMeta,
    pub headers: Option<HashMap<String, String>>,
    pub body: Option<Vec<u8>>,
}

impl TryFrom<JobEntry> for hyper::Request<hyper::Body> {
    type Error = Error;

    fn try_from(value: JobEntry) -> Result<Self, Self::Error> {
        let JobProtocol::Http(meta) = value.meta.protocol else {
            return Err(Error::InvalidUri);
        };
        let body = match value.body {
            Some(v) => hyper::Body::from(v),
            None => hyper::Body::empty(),
        };
        let mut req = hyper::Request::builder().method(meta.method).uri(meta.url);
        if let Some(headers) = value.headers {
            let map = HeaderMap::try_from(&headers)?;
            let headers_mut = req.headers_mut().unwrap();
            for (key, value) in map {
                if let Some(key) = key {
                    headers_mut.insert(key, value);
                }
            }
        };
        Ok(req.body(body)?)
    }
}

#[tokio::test]
async fn job_entry_into_request_err() -> anyhow::Result<()> {
    // arrange
    let job_entry = JobEntry {
        id: 0,
        meta: JobMeta::default(),
        headers: None,
        body: None,
    };
    // act
    let req = hyper::Request::<hyper::Body>::try_from(job_entry);

    // assert
    assert!(req.is_err());

    Ok(())
}

#[tokio::test]
async fn job_entry_into_request_ok() -> anyhow::Result<()> {
    // arrange
    let job_entry = JobEntry {
        id: 0,
        meta: JobMeta {
            protocol: JobProtocol::Http(HttpMeta {
                method: Method::GET,
                url: Uri::try_from("http://localhost").unwrap(),
            }),
            retry: JobRetryBackoff::Fixed {
                retry: 3,
                retry_delay: 1,
            },
            delay: Some(300),
        },
        headers: Some(HashMap::from([(
            header::CONTENT_LENGTH.to_string(),
            "123".into(),
        )])),
        body: None,
    };
    // act
    let req = hyper::Request::<hyper::Body>::try_from(job_entry);

    // assert
    assert!(req.is_ok());

    Ok(())
}
