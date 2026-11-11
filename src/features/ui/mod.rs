//! Server-rendered UI: Askama templates, htmx for the partial updates, Pico for the
//! base styles. Every live region is a partial with a stable id, so the same templates
//! and endpoints would serve another hypermedia library (Datastar) as well.

mod assets;
mod csrf;
pub mod form;
mod instances;
mod jobs;
mod login;
mod schedules;
pub mod views;

use crate::{
    handlers::http::create_job,
    models::{AppState, Error},
};
use askama::Template;
use askama_web::WebTemplate;
use axum::{
    Router,
    body::Bytes,
    extract::{DefaultBodyLimit, RawQuery, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware,
    response::{IntoResponse, Redirect, Response},
    routing::{get, post},
};
use chrono::Utc;
use form::{Created, JobForm};
use std::{collections::BTreeMap, sync::Arc};
use tracing::error;

/// No inline scripts or styles anywhere in the UI: everything comes from `/ui/assets`.
const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; \
    connect-src 'self'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'";

/// The header of a page: the current section, and the logout when the UI asks for a token.
#[derive(Debug, Clone, Copy)]
pub struct Nav {
    pub current: &'static str,
    pub logout: bool,
}

impl Nav {
    pub fn new(state: &AppState, current: &'static str) -> Self {
        Nav {
            current,
            logout: state.api_tokens.is_some(),
        }
    }
}

#[derive(Template, WebTemplate)]
#[template(path = "ui/new_job.html")]
struct NewJobPage {
    nav: Nav,
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
        .route("/ui", get(|| async { Redirect::to("/ui/jobs") }))
        .route(
            "/ui/jobs",
            get(jobs::jobs_page)
                .post(create_job_from_form)
                .layer(DefaultBodyLimit::max(state.max_body_size)),
        )
        .route("/ui/jobs/live", get(jobs::jobs_live))
        .route("/ui/jobs/new", get(new_job_page))
        .route("/ui/jobs/new/form", get(new_job_form))
        .route("/ui/jobs/new/preview", get(new_job_preview))
        .route(
            "/ui/jobs/{id}",
            get(jobs::job_page).delete(jobs::job_delete),
        )
        .route("/ui/jobs/{id}/history", get(jobs::job_history))
        .route("/ui/jobs/{id}/retry", post(jobs::job_retry))
        .route("/ui/jobs/{id}/cancel", post(jobs::job_cancel))
        .route("/ui/jobs/{id}/delete", post(jobs::job_delete))
        .route("/ui/schedules", get(schedules::schedules_page))
        .route("/ui/instances", get(instances::instances_page))
        .route("/ui/instances/table", get(instances::instances_table))
        .route("/ui/schedules/{id}/pause", post(schedules::schedule_pause))
        .route(
            "/ui/schedules/{id}/resume",
            post(schedules::schedule_resume),
        )
        .route(
            "/ui/schedules/{id}/delete",
            post(schedules::schedule_delete),
        )
        .route(
            "/ui/login",
            get(login::login_page).post(login::login_submit),
        )
        .route("/ui/logout", post(login::logout))
        .route("/ui/assets/{name}", get(assets::asset))
        .route("/favicon.ico", get(assets::favicon))
        .route_layer(middleware::from_fn_with_state(
            Arc::clone(&state),
            login::require_login,
        ))
        // outermost: a cross-site post is refused before anything else
        .route_layer(middleware::from_fn(csrf::same_origin))
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

/// htmx asks for a partial, a plain browser request for a page or a redirect.
fn is_htmx(headers: &HeaderMap) -> bool {
    headers.contains_key("hx-request")
}

/// The details go to the log, not to the browser.
fn server_error(err: &Error) -> Response {
    error!("ui error {:?}", err);
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        "Something went wrong, see the logs.",
    )
        .into_response()
}

fn page(state: &AppState, form: JobForm) -> NewJobPage {
    NewJobPage {
        nav: Nav::new(state, "new"),
        form,
    }
}

async fn new_job_page(State(state): State<Arc<AppState>>) -> NewJobPage {
    page(
        &state,
        JobForm::default().with_limits(&state.worker_options),
    )
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
    let form = JobForm::from_pairs(&body).with_limits(&state.worker_options);
    let outcome = match form.prepare(&state.worker_options, Utc::now()) {
        Err(errors) => Err(errors),
        Ok(job) => create_job(&state, job).await.map_err(|err| {
            error!("ui create_job error {:?}", err);
            BTreeMap::from([("form", "it could not be stored, try again.".to_owned())])
        }),
    };
    let (status, form, trigger) = match outcome {
        Ok(row) => {
            let mut fresh = JobForm::default().with_limits(&state.worker_options);
            fresh.created = Some(Created {
                id: row.id,
                schedule_id: row.schedule_id,
            });
            (StatusCode::OK, fresh, Some([("hx-trigger", "job-created")]))
        }
        Err(errors) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            JobForm { errors, ..form },
            None,
        ),
    };
    if is_htmx(&headers) {
        (status, trigger, JobFormPartial { form }).into_response()
    } else {
        (status, trigger, page(&state, form)).into_response()
    }
}
