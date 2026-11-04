//! Instances: the irisqo processes sharing the queue, with their heartbeat and work.

use std::sync::Arc;

use askama::Template;
use askama_web::WebTemplate;
use axum::{
    extract::State,
    response::{IntoResponse, Response},
};
use chrono::{DateTime, Utc};

use super::{
    server_error,
    views::{relative, utc},
};
use crate::{
    features::instances::{self, InstanceView},
    models::{AppState, Error},
    services::heartbeat::{HEARTBEAT_INTERVAL, INSTANCE_EXPIRE},
};

pub struct InstanceRowView {
    pub id: String,
    pub host: String,
    pub ulid: String,
    /// `running`, `stale` (live without a recent heartbeat) or `stopped`.
    pub state: &'static str,
    pub state_label: &'static str,
    pub seen: String,
    pub seen_title: String,
    pub started: String,
    pub started_title: String,
    pub processing: i64,
    pub completed_24h: i64,
    pub failed_24h: i64,
    pub this: bool,
}

impl InstanceRowView {
    fn new(view: &InstanceView, now: DateTime<Utc>, this_id: &str) -> Self {
        let (host, ulid) = view
            .instance_id
            .rsplit_once(':')
            .unwrap_or(("", view.instance_id.as_str()));
        let quiet = (now - view.last_at).num_seconds();
        let (state, state_label) = match view.status.as_str() {
            "live" if quiet > HEARTBEAT_INTERVAL.as_secs() as i64 * 2 => ("stale", "No heartbeat"),
            "live" => ("running", "Live"),
            _ => ("stopped", "Stopped"),
        };
        // the instance id ends with a ULID made at start
        let started_at = ulid::Ulid::from_string(ulid)
            .ok()
            .map(|u| DateTime::<Utc>::from(u.datetime()));
        InstanceRowView {
            id: view.instance_id.clone(),
            host: host.to_owned(),
            ulid: ulid.to_owned(),
            state,
            state_label,
            seen: relative(-quiet),
            seen_title: utc(view.last_at),
            started: started_at
                .map(|at| relative((at - now).num_seconds()))
                .unwrap_or_default(),
            started_title: started_at.map(utc).unwrap_or_default(),
            processing: view.processing,
            completed_24h: view.completed_24h,
            failed_24h: view.failed_24h,
            this: view.instance_id == this_id,
        }
    }
}

#[derive(Template, WebTemplate)]
#[template(path = "ui/instances.html")]
pub struct InstancesPage {
    nav: &'static str,
    rows: Vec<InstanceRowView>,
    heartbeat_secs: u64,
    expire_secs: u64,
}

#[derive(Template, WebTemplate)]
#[template(path = "ui/partials/instances_table.html")]
pub struct InstancesPartial {
    rows: Vec<InstanceRowView>,
}

async fn load(state: &AppState) -> Result<Vec<InstanceRowView>, Error> {
    let now = Utc::now();
    let rows = instances::list_view(&state.pool, 200).await?;
    Ok(rows
        .iter()
        .map(|row| InstanceRowView::new(row, now, &state.instance_id))
        .collect())
}

pub async fn instances_page(State(state): State<Arc<AppState>>) -> Response {
    match load(&state).await {
        Ok(rows) => InstancesPage {
            nav: "instances",
            rows,
            heartbeat_secs: HEARTBEAT_INTERVAL.as_secs(),
            expire_secs: INSTANCE_EXPIRE.as_secs(),
        }
        .into_response(),
        Err(err) => server_error(&err),
    }
}

/// The table alone, refreshed every 5 s by the page.
pub async fn instances_table(State(state): State<Arc<AppState>>) -> Response {
    match load(&state).await {
        Ok(rows) => InstancesPartial { rows }.into_response(),
        Err(err) => server_error(&err),
    }
}
