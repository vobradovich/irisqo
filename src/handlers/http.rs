use crate::{
    db,
    models::{AppState, Error, JobCreate, JobMeta},
};
use axum::{
    extract::{Path, RawQuery, State},
    http::{header, HeaderMap, StatusCode},
    response::IntoResponse,
};
use hyper::Uri;
use problemdetails::Problem;
use std::sync::Arc;
#[allow(unused_imports)]
use tracing::{debug, error, info, warn};

pub async fn create(
    State(state): State<Arc<AppState>>,
    Path(url): Path<String>,
    RawQuery(query): RawQuery,
    // body: RawBody
) -> Result<impl IntoResponse, Problem> {
    let uri = Uri::try_from(url).map_err(|_| Error::InvalidUri)?;
    let protocol = uri.scheme_str().unwrap_or("null");
    let full_url = match query {
        Some(qs) => format!("{uri}?{qs}"),
        None => uri.to_string(),
    };
    let job_create = JobCreate {
        meta: JobMeta::default(),
        headers: None,
        body: None,
        at: None,
    };
    debug!("{:?}", serde_json::to_string(&job_create.meta));
    let id = db::jobs::enqueue(&state.pool, protocol, full_url.as_str()).await?;
    let mut headers = HeaderMap::new();
    headers.insert(
        header::LOCATION,
        format!("/api/v1/jobs/{}", id).parse().unwrap(),
    );
    headers.insert("job-id", id.into());
    Ok((StatusCode::CREATED, headers))
}
