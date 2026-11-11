//! Schedules: the list with pause, resume and delete.

use std::sync::Arc;

use askama::Template;
use askama_web::WebTemplate;
use axum::{
    extract::{Path, State},
    http::HeaderMap,
    response::{IntoResponse, Redirect, Response},
};
use chrono::{DateTime, Utc};

use super::{
    Nav, is_htmx, server_error,
    views::{filters, relative, utc},
};
use crate::{
    db::jobview::JobStatus,
    features::schedules::{self, JobSchedule, ScheduleRule, ScheduleView},
    models::{AppState, Error, JobSink},
};

pub struct ScheduleRowView {
    pub id: String,
    pub method: String,
    pub url: String,
    pub rule: String,
    pub rule_hint: String,
    pub next: String,
    pub next_title: String,
    /// The status class of the last run.
    pub last_status: &'static str,
    pub last_label: &'static str,
    pub last_run_at: String,
    pub last_id: Option<i64>,
    pub active: bool,
}

fn rule_hint(rule: &str, tz: &str, until: Option<DateTime<Utc>>) -> String {
    let base = match JobSchedule::from_row(rule, tz, until) {
        Ok(JobSchedule {
            rule: ScheduleRule::Interval(interval),
            ..
        }) => match interval {
            i if i % 86_400 == 0 => format!("every {} d", i / 86_400),
            i if i % 3600 == 0 => format!("every {} h", i / 3600),
            i if i % 60 == 0 => format!("every {} min", i / 60),
            i => format!("every {i} s"),
        },
        Ok(JobSchedule { tz, .. }) => format!("cron, {}", tz.name()),
        Err(_) => "invalid".into(),
    };
    match until {
        Some(until) => format!("{base}, until {}", until.format("%Y-%m-%d %H:%M")),
        None => base,
    }
}

impl ScheduleRowView {
    fn new(view: &ScheduleView, now: DateTime<Utc>) -> Self {
        let (method, url) = match view.meta.as_ref().map(|m| &m.sink) {
            Some(JobSink::Http(http)) => (http.method.to_string(), http.url.to_string()),
            Some(JobSink::Noop) | None => ("-".into(), "noop".into()),
        };
        let (next, next_title) = match (view.pending, view.next_at) {
            // overdue: how long ago it was due, until the scheduler picks it up
            (true, Some(at)) => (relative((at - now).num_seconds()), utc(at)),
            (true, None) => ("pending".into(), String::new()),
            (false, _) if view.paused => ("-".into(), "paused".into()),
            (false, _) => ("-".into(), "no next run".into()),
        };
        let (last_status, last_label) = match view.last_status.clone().map(JobStatus::from) {
            Some(status) => (status.as_str(), status.label()),
            None => (JobStatus::Cancelled.as_str(), "Not yet"),
        };
        ScheduleRowView {
            id: view.schedule_id.clone(),
            method,
            url,
            rule: view.rule.clone(),
            rule_hint: rule_hint(&view.rule, &view.tz, view.until),
            next,
            next_title,
            last_status,
            last_label,
            last_run_at: view
                .last_run_at
                .map(|at| relative((at - now).num_seconds()))
                .unwrap_or_default(),
            last_id: view.last_id,
            active: !view.paused,
        }
    }
}

#[derive(Template, WebTemplate)]
#[template(path = "ui/schedules.html")]
pub struct SchedulesPage {
    nav: Nav,
    rows: Vec<ScheduleRowView>,
}

#[derive(Template, WebTemplate)]
#[template(path = "ui/partials/schedules_table.html")]
pub struct SchedulesPartial {
    rows: Vec<ScheduleRowView>,
}

async fn load(state: &AppState) -> Result<Vec<ScheduleRowView>, Error> {
    let now = Utc::now();
    let rows = schedules::list_view(&state.pool, 200).await?;
    Ok(rows
        .iter()
        .map(|row| ScheduleRowView::new(row, now))
        .collect())
}

pub async fn schedules_page(State(state): State<Arc<AppState>>) -> Response {
    match load(&state).await {
        Ok(rows) => SchedulesPage {
            nav: Nav::new(&state, "schedules"),
            rows,
        }
        .into_response(),
        Err(err) => server_error(&err),
    }
}

#[derive(Clone, Copy)]
enum Action {
    Pause,
    Resume,
    Delete,
}

async fn act(state: &AppState, id: &str, headers: &HeaderMap, action: Action) -> Response {
    let done = match action {
        Action::Pause => schedules::pause(&state.pool, id, &state.instance_id)
            .await
            .map(drop),
        Action::Resume => schedules::resume(&state.pool, id, Utc::now(), &state.instance_id)
            .await
            .map(drop),
        Action::Delete => schedules::db::delete(&state.pool, id).await.map(drop),
    };
    if let Err(err) = done {
        return server_error(&err);
    }
    if !is_htmx(headers) {
        return Redirect::to("/ui/schedules").into_response();
    }
    match load(state).await {
        Ok(rows) => SchedulesPartial { rows }.into_response(),
        Err(err) => server_error(&err),
    }
}

pub async fn schedule_pause(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    act(&state, &id, &headers, Action::Pause).await
}

pub async fn schedule_resume(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    act(&state, &id, &headers, Action::Resume).await
}

pub async fn schedule_delete(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    act(&state, &id, &headers, Action::Delete).await
}
