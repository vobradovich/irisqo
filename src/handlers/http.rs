use crate::{
    db,
    models::{AppState, Error, HttpMeta, JobCreate, JobMeta},
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
use std::{collections::HashMap, sync::Arc};
#[allow(unused_imports)]
use tracing::{debug, error, info, warn};

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
    let url = match query {
        Some(qs) => format!("{url}?{qs}"),
        None => url,
    };
    let uri = Uri::try_from(url.as_str()).map_err(|_| Error::InvalidUri)?;
    let scheme = &uri.scheme_str().unwrap_or("null").to_owned();
    let protocol = match scheme.as_str() {
        "http" | "https" => crate::models::JobProtocol::Http(HttpMeta { method, url: uri }),
        _ => crate::models::JobProtocol::Null,
    };

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
            protocol: protocol,
            retry: crate::models::JobRetry::None,
            delay: None,
            timeout: None,
        },
        headers: Some(header_hashmap),
        body: body,
        at: None,
    };

    debug!("{:?}", serde_json::to_string(&job_create.meta));
    let id = db::jobs::enqueue(&state.pool, job_create).await?;
    let mut headers = HeaderMap::new();
    headers.insert(
        header::LOCATION,
        format!("/api/v1/jobs/{}", id).parse().unwrap(),
    );
    headers.insert("job-id", id.into());
    Ok((StatusCode::CREATED, headers))
}
