use crate::{
    db,
    features::schedules::JobSchedule,
    models::{AppState, Error, HttpMeta, JobCreate, JobMeta, JobRetry},
    otel,
};
use axum::{
    Router,
    body::Bytes,
    extract::{Path, RawQuery, State},
    http::{HeaderMap, StatusCode, header},
    response::IntoResponse,
    routing::get,
};
use hyper::{Method, Uri};
use problemdetails::Problem;

use std::{collections::HashMap, sync::Arc};
#[allow(unused_imports)]
use tracing::{debug, error, info, warn};
use url::{Url, form_urlencoded};

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
    let mut at: Option<i64> = None;
    let mut timeout: u32 = state.worker_options.timeout;
    let mut retry: JobRetry = JobRetry::None;
    let mut schedule: Option<JobSchedule> = None;
    let mut until: Option<i64> = None;
    let mut external_id: Option<String> = None;

    // Parse and truncate Query String
    let mut parsed_url = Url::parse(&url).map_err(|_| Error::InvalidUrl)?;
    if let Some(qs) = query {
        let params = form_urlencoded::parse(qs.as_bytes());
        let now_secs = JobSchedule::now_secs();
        for (key, value) in params {
            if key == "_delay" {
                delay = value.parse::<u32>().ok();
                at = delay.map(|t| now_secs + i64::from(t));
                continue;
            }
            if key == "_delay_until" {
                at = value.parse::<i64>().ok().filter(|&t| t > now_secs);
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
            if key == "_interval" {
                let job_schedule = value
                    .parse()
                    .map_err(|_| Error::InvalidParams("interval"))?;
                schedule = Some(job_schedule);
                continue;
            }
            if key == "_cron" {
                let job_schedule = value.parse().map_err(|_| Error::InvalidParams("cron"))?;
                schedule = Some(job_schedule);
                continue;
            }
            if key == "_until" {
                until = value.parse::<i64>().ok();
                continue;
            }
            if key == "_id" && !value.is_empty() && value.len() < 65 {
                external_id = Some(value.to_string());
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
    let scheme = uri.scheme_str();
    let protocol = match scheme {
        Some("http") | Some("https") => {
            crate::models::JobProtocol::Http(HttpMeta { method, url: uri })
        }
        _ => crate::models::JobProtocol::None,
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
    // OpenTelemetry TraceId
    let trace_id = otel::current_trace_id();
    // Build
    let job_create = JobCreate {
        meta: JobMeta {
            protocol,
            retry,
            delay,
            timeout,
            trace_id,
        },
        headers: Some(header_hashmap),
        body,
        at,
        schedule,
        until,
        external_id,
    };

    debug!("{:?}", serde_json::to_string(&job_create.meta));
    let job = db::jobqueue::create(&state.pool, job_create, &state.instance_id).await?;
    let mut headers = HeaderMap::new();
    headers.insert(
        header::LOCATION,
        format!("/api/v1/jobs/{}", job.id).parse().unwrap(),
    );
    headers.insert("job-id", job.id.into());
    if let Some(schedule_id) = job.schedule_id {
        headers.insert("schedule-id", schedule_id.parse().unwrap());
    }
    if let Some(external_id) = job.external_id {
        headers.insert("external-id", external_id.parse().unwrap());
    }
    Ok((StatusCode::CREATED, headers))
}
