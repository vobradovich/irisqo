use std::{collections::HashMap, time::SystemTime};

use axum::body::Bytes;
use hyper::HeaderMap;
#[cfg(test)]
use hyper::{header, Method, Uri};

use serde::{Deserialize, Serialize};
use sqlx::types::Json;

use super::{Error, JobRetry};

#[derive(Debug, Clone, sqlx::FromRow, Serialize)]
pub struct JobRow {
    pub id: i64,
    pub protocol: String,
    pub meta: Json<JobMeta>,
    pub headers: Option<Json<HashMap<String, String>>>,
    pub body: Option<Vec<u8>>,
}

#[derive(Debug, sqlx::FromRow)]
pub struct JobQueueRow {
    pub id: i64,
    pub retry: i32,
}

#[derive(Debug, Clone, Copy, sqlx::FromRow)]
pub struct JobEntry {
    pub id: i64,
    pub retry: i32,
}


#[derive(Debug, Clone, Default)]
pub struct JobCreate {
    pub meta: JobMeta,
    pub headers: Option<HashMap<String, String>>,
    pub body: Bytes,
    pub at: Option<u64>,
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
#[serde(tag = "schedule")]
#[serde(rename_all = "snake_case")]
pub enum JobSchedule {
    #[default]
    None,
    Delay {
        delay: u32,
    },
    At {
        at: Option<SystemTime>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HttpMeta {
    #[serde(with = "http_serde::method")]
    pub method: hyper::Method,
    #[serde(with = "http_serde::uri")]
    pub url: hyper::Uri,
}

impl TryFrom<JobRow> for hyper::Request<hyper::Body> {
    type Error = Error;

    fn try_from(value: JobRow) -> Result<Self, Self::Error> {
        let meta = value.meta.0;
        let JobProtocol::Http(meta) = meta.protocol else {
            return Err(Error::InvalidUrl);
        };
        let body = match value.body {
            Some(v) => hyper::Body::from(v),
            None => hyper::Body::empty(),
        };
        let mut req = hyper::Request::builder().method(meta.method).uri(meta.url);
        if let Some(headers) = value.headers {
            let headers = headers.as_ref();
            let map = HeaderMap::try_from(headers)?;
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
async fn job_row_into_request_err() -> anyhow::Result<()> {
    // arrange
    let job_entry = JobRow {
        id: 0,
        protocol: "null".into(),
        meta: Json(JobMeta::default()),
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
async fn job_row_into_request_ok() -> anyhow::Result<()> {
    // arrange
    let job_entry = JobRow {
        id: 0,
        protocol: "http".into(),
        meta: Json(JobMeta {
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
        }),
        headers: Some(Json(HashMap::from([(
            header::CONTENT_LENGTH.to_string(),
            "123".into(),
        )]))),
        body: None,
    };
    // act
    let req = hyper::Request::<hyper::Body>::try_from(job_entry);

    // assert
    assert!(req.is_ok());

    Ok(())
}
