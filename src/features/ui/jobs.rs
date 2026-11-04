//! Jobs: the live list, a job page and its actions.

use std::sync::Arc;

use askama::Template;
use askama_web::WebTemplate;
use axum::{
    extract::{Path, RawQuery, State},
    http::{HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Redirect, Response},
};
use chrono::Utc;
use url::form_urlencoded;

use super::{
    Nav, is_htmx, server_error,
    views::{JobDetailView, JobRowView, filters, relative},
};
use crate::{
    db::{
        self,
        jobqueue::ActionOutcome,
        jobview::{JobQuery, JobStatus, ScheduleFilter, StatusFilter},
    },
    features::{history, results},
    models::{AppState, Error},
};

const PAGE_SIZE: i32 = 50;

pub struct StatCard {
    pub label: &'static str,
    pub value: String,
    pub hint: String,
}

pub struct FilterLink {
    pub label: &'static str,
    pub href: String,
    pub current: bool,
}

/// The list as the request asks for it, and the links to change it.
pub struct JobsList {
    pub status: &'static str,
    pub schedule: &'static str,
    pub q: String,
    pub live: bool,
    pub live_href: String,
    pub pause_href: String,
    pub resume_href: String,
    pub newest_href: Option<String>,
    pub older_href: Option<String>,
    pub filters: Vec<FilterLink>,
    pub stats: Vec<StatCard>,
    pub rows: Vec<JobRowView>,
    /// Announced to screen readers when it changes (the list itself is not a live region:
    /// its times change every refresh).
    pub summary: String,
}

#[derive(Debug, Clone)]
struct ListParams {
    status: StatusFilter,
    schedule: ScheduleFilter,
    q: String,
    before: Option<i64>,
    live: bool,
}

impl Default for ListParams {
    fn default() -> Self {
        ListParams {
            status: StatusFilter::default(),
            schedule: ScheduleFilter::default(),
            q: String::new(),
            before: None,
            live: true,
        }
    }
}

impl ListParams {
    fn parse(query: Option<&str>) -> Self {
        let mut params = ListParams::default();
        for (key, value) in form_urlencoded::parse(query.unwrap_or_default().as_bytes()) {
            match key.as_ref() {
                "status" => params.status = StatusFilter::parse(&value),
                "schedule" => params.schedule = ScheduleFilter::parse(&value),
                "q" => params.q = value.trim().to_owned(),
                "before" => params.before = value.parse().ok(),
                "live" => params.live = value != "0",
                _ => {}
            }
        }
        params
    }

    fn query(&self, status: StatusFilter, before: Option<i64>, live: bool) -> String {
        let mut out = form_urlencoded::Serializer::new(String::new());
        out.append_pair("status", status.as_str());
        if self.schedule != ScheduleFilter::Any {
            out.append_pair("schedule", self.schedule.as_str());
        }
        if !self.q.is_empty() {
            out.append_pair("q", &self.q);
        }
        if let Some(before) = before {
            out.append_pair("before", &before.to_string());
        }
        if !live {
            out.append_pair("live", "0");
        }
        out.finish()
    }
}

#[derive(Template, WebTemplate)]
#[template(path = "ui/jobs.html")]
pub struct JobsPage {
    nav: Nav,
    list: JobsList,
}

#[derive(Template, WebTemplate)]
#[template(path = "ui/partials/jobs_live.html")]
pub struct JobsLivePartial {
    list: JobsList,
}

#[derive(Template, WebTemplate)]
#[template(path = "ui/partials/job_history.html")]
pub struct JobHistoryPartial {
    id: i64,
    history: Vec<history::JobHistoryRow>,
}

#[derive(Template, WebTemplate)]
#[template(path = "ui/job.html")]
pub struct JobPage {
    nav: Nav,
    job: JobDetailView,
}

#[derive(Template, WebTemplate)]
#[template(path = "ui/partials/job_detail.html")]
pub struct JobDetailPartial {
    job: JobDetailView,
}

#[derive(Template, WebTemplate)]
#[template(path = "ui/not_found.html")]
pub struct NotFoundPage {
    nav: Nav,
    what: String,
}

fn not_found(state: &AppState, what: String) -> Response {
    let nav = Nav::new(state, "");
    (StatusCode::NOT_FOUND, NotFoundPage { nav, what }).into_response()
}

async fn load_list(state: &AppState, params: &ListParams) -> Result<JobsList, Error> {
    let query = JobQuery {
        status: params.status,
        schedule: params.schedule,
        search: (!params.q.is_empty()).then(|| params.q.clone()),
        before: params.before,
        limit: PAGE_SIZE,
    };
    let (jobs, queue, recent) = tokio::try_join!(
        db::jobview::list(&state.pool, &query),
        db::stats::queue_stats(&state.pool),
        db::jobview::recent_stats(&state.pool),
    )?;
    let now = Utc::now();
    let rows: Vec<JobRowView> = jobs.iter().map(|job| JobRowView::new(job, now)).collect();
    // live updates only on the first page
    let live = params.live && params.before.is_none();
    let older_href = (rows.len() == usize::try_from(PAGE_SIZE).unwrap_or(usize::MAX))
        .then(|| rows.last().map(|last| last.id))
        .flatten()
        .map(|id| {
            format!(
                "/ui/jobs?{}",
                params.query(params.status, Some(id), params.live)
            )
        });
    let filters = StatusFilter::ALL
        .into_iter()
        .map(|filter| FilterLink {
            label: filter.label(),
            href: format!("/ui/jobs?{}", params.query(filter, None, params.live)),
            current: filter == params.status,
        })
        .collect();
    let stats = vec![
        StatCard {
            label: "Scheduled",
            value: queue.scheduled.to_string(),
            hint: "waiting for their time".into(),
        },
        StatCard {
            label: "Enqueued",
            value: queue.enqueued.to_string(),
            hint: "waiting for a worker".into(),
        },
        StatCard {
            label: "Processing",
            value: queue.processing.to_string(),
            hint: format!("on {} live instances", recent.instances),
        },
        StatCard {
            label: "Failed, 24 h",
            value: recent.failed_24h.to_string(),
            hint: format!("of {} processed", recent.processed_24h),
        },
        StatCard {
            label: "Queue lag",
            value: if queue.lag < 10.0 {
                format!("{:.1} s", queue.lag)
            } else {
                relative(-(queue.lag.max(0.0) as i64))
                    .trim_end_matches(" ago")
                    .to_owned()
            },
            hint: "oldest due job waiting".into(),
        },
    ];
    let status_word = match params.status {
        StatusFilter::All => String::new(),
        filter @ (StatusFilter::Active | StatusFilter::Only(_)) => format!("{} ", filter.as_str()),
    };
    Ok(JobsList {
        status: params.status.as_str(),
        schedule: params.schedule.as_str(),
        q: params.q.clone(),
        live,
        live_href: format!("/ui/jobs/live?{}", params.query(params.status, None, true)),
        pause_href: format!(
            "/ui/jobs?{}",
            params.query(params.status, params.before, false)
        ),
        resume_href: format!("/ui/jobs?{}", params.query(params.status, None, true)),
        newest_href: params.before.map(|_| {
            format!(
                "/ui/jobs?{}",
                params.query(params.status, None, params.live)
            )
        }),
        older_href,
        filters,
        stats,
        summary: format!("{} {status_word}jobs shown", rows.len()),
        rows,
    })
}

pub async fn jobs_page(State(state): State<Arc<AppState>>, RawQuery(query): RawQuery) -> Response {
    let params = ListParams::parse(query.as_deref());
    match load_list(&state, &params).await {
        Ok(list) => JobsPage {
            nav: Nav::new(&state, "jobs"),
            list,
        }
        .into_response(),
        Err(err) => server_error(&err),
    }
}

/// The stats and the list, polled by the page (morphed in place).
pub async fn jobs_live(State(state): State<Arc<AppState>>, RawQuery(query): RawQuery) -> Response {
    let params = ListParams::parse(query.as_deref());
    match load_list(&state, &params).await {
        Ok(list) => JobsLivePartial { list }.into_response(),
        Err(err) => server_error(&err),
    }
}

pub async fn job_history(State(state): State<Arc<AppState>>, Path(id): Path<i64>) -> Response {
    match history::get_by_id(&state.pool, id, 200, 0).await {
        Ok(history) => JobHistoryPartial { id, history }.into_response(),
        Err(err) => server_error(&err),
    }
}

async fn load_job(state: &AppState, id: i64) -> Result<Option<JobDetailView>, Error> {
    let (view, job, result, history) = tokio::try_join!(
        db::jobview::get(&state.pool, id),
        db::jobqueue::get_by_id(&state.pool, id),
        results::get_by_id(&state.pool, id),
        history::get_by_id(&state.pool, id, 500, 0),
    )?;
    let (Some(view), Some(job)) = (view, job) else {
        return Ok(None);
    };
    let result = result.map(results::JobResult::from);
    Ok(Some(JobDetailView::new(
        &view,
        &job,
        result.as_ref(),
        history,
        state.trace_url.as_deref(),
        Utc::now(),
    )))
}

pub async fn job_page(State(state): State<Arc<AppState>>, Path(id): Path<i64>) -> Response {
    match load_job(&state, id).await {
        Ok(Some(job)) => JobPage {
            nav: Nav::new(&state, "jobs"),
            job,
        }
        .into_response(),
        Ok(None) => not_found(&state, format!("Job {id}")),
        Err(err) => server_error(&err),
    }
}

#[derive(Clone, Copy)]
enum Action {
    Retry,
    Cancel,
}

async fn act(state: &AppState, id: i64, headers: &HeaderMap, action: Action) -> Response {
    let outcome = match action {
        Action::Retry => db::jobqueue::requeue_job(&state.pool, id, &state.instance_id).await,
        Action::Cancel => db::jobqueue::cancel_job(&state.pool, id, &state.instance_id).await,
    };
    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(err) => return server_error(&err),
    };
    if !is_htmx(headers) {
        return Redirect::to(&format!("/ui/jobs/{id}")).into_response();
    }
    let job = match load_job(state, id).await {
        Ok(Some(job)) => job,
        Ok(None) => return not_found(state, format!("Job {id}")),
        Err(err) => return server_error(&err),
    };
    let notice = match (action, outcome) {
        (Action::Retry, ActionOutcome::Done) => {
            ("success", "Queued again: it runs now.".to_owned())
        }
        (Action::Cancel, ActionOutcome::Done) => ("success", "Cancelled.".to_owned()),
        (_, ActionOutcome::Conflict) => (
            "error",
            "Not possible any more: the job changed meanwhile.".to_owned(),
        ),
        (_, ActionOutcome::NotFound) => ("error", "The job does not exist.".to_owned()),
    };
    let status = match outcome {
        ActionOutcome::Done => StatusCode::OK,
        ActionOutcome::Conflict | ActionOutcome::NotFound => StatusCode::CONFLICT,
    };
    let mut job = job;
    job.notice = Some(notice);
    (status, JobDetailPartial { job }).into_response()
}

pub async fn job_retry(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> Response {
    act(&state, id, &headers, Action::Retry).await
}

pub async fn job_cancel(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> Response {
    act(&state, id, &headers, Action::Cancel).await
}

/// `DELETE` from htmx (then `HX-Redirect`), `POST .../delete` from a plain form.
pub async fn job_delete(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> Response {
    if let Err(err) = db::jobqueue::delete(&state.pool, id).await {
        return server_error(&err);
    }
    if is_htmx(&headers) {
        // 200, not 204: htmx skips 204 responses
        let mut response = StatusCode::OK.into_response();
        response
            .headers_mut()
            .insert("hx-redirect", HeaderValue::from_static("/ui/jobs"));
        response
    } else {
        Redirect::to("/ui/jobs").into_response()
    }
}
