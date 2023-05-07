use crate::{
    db,
    models::{AppState, Error, HttpMeta, JobCreate, JobMeta, JobRetry},
};
use axum::{
    body::Bytes,
    extract::{Path, RawQuery, State},
    http::{header, HeaderMap, StatusCode},
    response::IntoResponse,
    routing::get,
    Router,
};
use hyper::{Method, Uri};
use problemdetails::Problem;

use std::{
    collections::HashMap,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
#[allow(unused_imports)]
use tracing::{debug, error, info, warn};
use url::{form_urlencoded, Url};

pub fn routes(state: Arc<AppState>) -> Router {
    Router::new()
        .route(
            "/to/*url",
            get(job_create)
                .post(job_create)
                .put(job_create)
                .delete(job_create)
                .patch(job_create),
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
    let mut delay: Option<u32> = None;
    let mut at: Option<u64> = None;
    let mut timeout: u32 = state.worker_options.timeout;
    let mut retry: JobRetry = JobRetry::None;

    // Parse and truncate Query String
    let mut parsed_url = Url::parse(&url).map_err(|_| Error::InvalidUrl)?;
    if let Some(qs) = query {
        let params = form_urlencoded::parse(qs.as_bytes());
        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        for (key, value) in params {
            if key == "_delay" {
                delay = value.parse::<u32>().ok();
                at = delay.and_then(|t| (now_secs + u64::from(t)).try_into().ok());
                continue;
            }
            if key == "_delay_until" || key == "_until" {
                at = value
                    .parse::<u64>()
                    .ok()
                    .filter(|&t| t > now_secs && i64::try_from(t).is_ok());
                delay = at.and_then(|t| (t - now_secs).try_into().ok());
                continue;
            }
            if key == "_timeout" {
                timeout = value.parse::<u32>().unwrap_or(state.worker_options.timeout);
                continue;
            }
            if key == "_retry" {
                retry = value.parse()?;
                continue;
            }
            if value.is_empty() {
                parsed_url.query_pairs_mut().append_key_only(key.as_ref());
                continue;
            }
            parsed_url
                .query_pairs_mut()
                .append_pair(key.as_ref(), value.as_ref());
        }
    }
    let uri = Uri::try_from(parsed_url.as_str()).map_err(|_| Error::InvalidUrl)?;
    let scheme = &uri.scheme_str().unwrap_or("null").to_owned();
    let protocol = match scheme.as_str() {
        "http" | "https" => crate::models::JobProtocol::Http(HttpMeta { method, url: uri }),
        _ => crate::models::JobProtocol::Null,
    };

    // Parse Headers
    let mut header_hashmap = HashMap::new();
    for (k, v) in headers {
        if let Some(key) = k {
            if key == header::HOST {
                continue;
            }
            if let Ok(val) = v.to_str() {
                header_hashmap.insert(key.to_string(), val.to_owned());
            }
        }
    }

    let job_create = JobCreate {
        meta: JobMeta {
            protocol,
            retry,
            delay,
            timeout,
        },
        headers: Some(header_hashmap),
        body,
        at,
    };

    debug!("{:?}", serde_json::to_string(&job_create.meta));
    let id = db::jobqueue::enqueue(&state.pool, job_create).await?;
    let mut headers = HeaderMap::new();
    headers.insert(
        header::LOCATION,
        format!("/api/v1/jobs/{}", id).parse().unwrap(),
    );
    headers.insert("job-id", id.into());
    Ok((StatusCode::CREATED, headers))
}
