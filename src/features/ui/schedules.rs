//! Schedules: the list with pause, resume and delete.

use std::sync::Arc;

use askama::Template;
use askama_web::WebTemplate;
use axum::{
    extract::{Path, State},
    http::HeaderMap,
    response::{IntoResponse, Redirect, Response},
};
use chrono::{DateTime, TimeZone, Utc};

use super::{
    is_htmx, server_error,
    views::{relative, short_id, status_label, utc},
};
use crate::{
    features::schedules::{self, JobSchedule, ScheduleView},
    models::{AppState, Error, JobProtocol},
};

pub struct ScheduleRowView {
    pub id: String,
    pub id_short: String,
    pub method: String,
    pub url: String,
    pub rule: String,
    pub rule_hint: String,
    pub next: String,
    pub next_title: String,
    pub last_status: String,
    pub last_label: String,
    pub last_at: String,
    pub last_id: Option<i64>,
    pub active: bool,
}

fn rule_hint(schedule: &str, until: Option<i64>) -> String {
    let base = match schedule.parse::<JobSchedule>() {
        Ok(JobSchedule::Interval { interval }) => match interval {
            i if i % 86_400 == 0 => format!("every {} d", i / 86_400),
            i if i % 3600 == 0 => format!("every {} h", i / 3600),
            i if i % 60 == 0 => format!("every {} min", i / 60),
            i => format!("every {i} s"),
        },
        Ok(JobSchedule::Cron { .. }) => "cron, UTC".into(),
        Err(_) => "invalid".into(),
    };
    match until.and_then(|u| Utc.timestamp_opt(u, 0).single()) {
        Some(until) => format!("{base}, until {}", until.format("%Y-%m-%d %H:%M")),
        None => base,
    }
}

impl ScheduleRowView {
    fn new(view: &ScheduleView, now: DateTime<Utc>) -> Self {
        let (method, url) = match view.meta.as_ref().map(|m| &m.protocol) {
            Some(JobProtocol::Http(http)) => (http.method.to_string(), http.url.to_string()),
            _ => ("—".into(), "no protocol".into()),
        };
        let next_at = view
            .next_at
            .and_then(|secs| Utc.timestamp_opt(secs, 0).single());
        let (next, next_title) = match (view.pending, next_at) {
            (true, Some(at)) => {
                let delta = (at - now).num_seconds();
                (
                    if delta <= 0 {
                        "due".into()
                    } else {
                        relative(delta)
                    },
                    utc(at),
                )
            }
            (true, None) => ("pending".into(), String::new()),
            (false, _) if view.inactive => ("—".into(), "paused".into()),
            (false, _) => ("—".into(), "no next run".into()),
        };
        let last_status = view.last_status.clone().unwrap_or_default();
        ScheduleRowView {
            id: view.schedule_id.clone(),
            id_short: short_id(&view.schedule_id),
            method,
            url,
            rule: view.schedule.clone(),
            rule_hint: rule_hint(&view.schedule, view.until),
            next,
            next_title,
            last_label: if last_status.is_empty() {
                "Not yet".into()
            } else {
                status_label(&last_status)
            },
            last_status: if last_status.is_empty() {
                "cancelled".into()
            } else {
                last_status
            },
            last_at: view
                .last_at
                .map(|at| relative((at - now).num_seconds()))
                .unwrap_or_default(),
            last_id: view.last_id,
            active: !view.inactive,
        }
    }
}

#[derive(Template, WebTemplate)]
#[template(path = "ui/schedules.html")]
pub struct SchedulesPage {
    nav: &'static str,
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
            nav: "schedules",
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
        Action::Resume => {
            schedules::resume(&state.pool, id, JobSchedule::now_secs(), &state.instance_id)
                .await
                .map(drop)
        }
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
