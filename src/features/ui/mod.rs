//! Server-rendered UI: Askama templates, htmx for the partial updates, Pico for the
//! base styles. Every live region is a partial with a stable id, so the same templates
//! and endpoints would serve another hypermedia library (Datastar) as well.

mod assets;
pub mod form;

use crate::{features::schedules::JobSchedule, handlers::http::create_job, models::AppState};
use askama::Template;
use askama_web::WebTemplate;
use axum::{
    Router,
    body::Bytes,
    extract::{RawQuery, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware,
    response::{IntoResponse, Redirect, Response},
    routing::{get, post},
};
use form::{Created, JobForm};
use std::sync::Arc;
#[allow(unused_imports)]
use tracing::{debug, error, info, warn};

/// No inline scripts or styles anywhere in the UI: everything comes from `/ui/assets`.
const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; \
    connect-src 'self'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'";

#[derive(Template, WebTemplate)]
#[template(path = "ui/new_job.html")]
struct NewJobPage {
    nav: &'static str,
    form: JobForm,
}

#[derive(Template, WebTemplate)]
#[template(path = "ui/partials/job_form.html")]
struct JobFormPartial {
    form: JobForm,
}

#[derive(Template, WebTemplate)]
#[template(path = "ui/partials/job_preview.html")]
struct JobPreviewPartial {
    form: JobForm,
}

pub fn routes(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/ui", get(|| async { Redirect::to("/ui/jobs/new") }))
        .route("/ui/jobs/new", get(new_job_page))
        .route("/ui/jobs/new/form", get(new_job_form))
        .route("/ui/jobs/new/preview", get(new_job_preview))
        .route("/ui/jobs", post(create_job_from_form))
        .route("/ui/assets/{name}", get(assets::asset))
        .layer(middleware::map_response(security_headers))
        .with_state(state)
}

async fn security_headers(mut response: Response) -> Response {
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CSP),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("same-origin"),
    );
    response
}

fn page(form: JobForm) -> NewJobPage {
    NewJobPage { nav: "new", form }
}

async fn new_job_page(State(state): State<Arc<AppState>>) -> NewJobPage {
    page(JobForm::default().with_limits(&state.worker_options))
}

/// The whole form again, for a choice that shows other fields (`hx-get` on change).
async fn new_job_form(
    State(state): State<Arc<AppState>>,
    RawQuery(query): RawQuery,
) -> JobFormPartial {
    let form = JobForm::from_pairs(query.unwrap_or_default().as_bytes());
    JobFormPartial {
        form: form.with_limits(&state.worker_options),
    }
}

async fn new_job_preview(
    State(state): State<Arc<AppState>>,
    RawQuery(query): RawQuery,
) -> JobPreviewPartial {
    let form = JobForm::from_pairs(query.unwrap_or_default().as_bytes());
    JobPreviewPartial {
        form: form.with_limits(&state.worker_options),
    }
}

/// htmx gets the form partial, a plain form post the whole page; 422 when rejected
/// (htmx 4 swaps error responses too).
async fn create_job_from_form(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let htmx = headers.contains_key("hx-request");
    let mut form = JobForm::from_pairs(&body).with_limits(&state.worker_options);
    let created = match form.prepare(&state.worker_options, JobSchedule::now_secs()) {
        Err(errors) => {
            form.errors = errors;
            None
        }
        Ok(job) => match create_job(&state, job).await {
            Ok(row) => Some(row),
            Err(err) => {
                error!("ui create_job error {:?}", err);
                form.errors
                    .insert("form", "it could not be stored, try again.".into());
                None
            }
        },
    };
    let ok = created.is_some();
    if let Some(row) = created {
        form = JobForm::default().with_limits(&state.worker_options);
        form.created = Some(Created {
            id: row.id,
            schedule_id: row.schedule_id,
        });
    }
    let status = if ok {
        StatusCode::OK
    } else {
        StatusCode::UNPROCESSABLE_ENTITY
    };
    let mut response = if htmx {
        (status, JobFormPartial { form }).into_response()
    } else {
        (status, page(form)).into_response()
    };
    if ok {
        response
            .headers_mut()
            .insert("hx-trigger", HeaderValue::from_static("job-created"));
    }
    response
}
