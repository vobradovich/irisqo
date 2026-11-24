//! Instances: the irisqo processes sharing the queue, with their heartbeat and work.

use std::sync::Arc;

use askama::Template;
use askama_web::WebTemplate;
use axum::{
    extract::{RawQuery, State},
    response::{IntoResponse, Response},
};
use chrono::{DateTime, Utc};
use url::form_urlencoded;

use super::{
    Live, Nav,
    jobs::FilterLink,
    server_error,
    views::{filters, relative},
};
use crate::{
    features::instances::{self, InstanceFilter, InstanceView},
    models::{AppState, Error},
    models::{HEARTBEAT_INTERVAL, INSTANCE_EXPIRE},
};

pub struct InstanceRowView {
    pub id: String,
    pub host: String,
    pub ulid: String,
    pub version: String,
    /// What the process runs: `http`, `worker`, `scheduler`...
    pub roles: Vec<String>,
    /// `running`, `stale` (live without a recent heartbeat) or `stopped`.
    pub state: &'static str,
    pub state_label: &'static str,
    pub seen: String,
    pub seen_at: DateTime<Utc>,
    pub started: String,
    pub started_at: Option<DateTime<Utc>>,
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
        let quiet = (now - view.seen_at).num_seconds();
        let (state, state_label) = match view.stopped_at {
            None if quiet > i64::try_from(HEARTBEAT_INTERVAL.as_secs() * 2).unwrap_or(i64::MAX) => {
                ("stale", "No heartbeat")
            }
            None => ("running", "Live"),
            Some(_) => ("stopped", "Stopped"),
        };
        // the instance id ends with a ULID made at start
        let started_at = ulid::Ulid::from_string(ulid)
            .ok()
            .map(|u| DateTime::<Utc>::from(u.datetime()));
        InstanceRowView {
            id: view.instance_id.clone(),
            host: host.to_owned(),
            ulid: ulid.to_owned(),
            version: view.version.clone(),
            roles: view.roles.clone(),
            state,
            state_label,
            seen: relative(-quiet),
            seen_at: view.seen_at,
            started: started_at
                .map(|at| relative((at - now).num_seconds()))
                .unwrap_or_default(),
            started_at,
            processing: view.processing,
            completed_24h: view.completed_24h,
            failed_24h: view.failed_24h,
            this: view.instance_id == this_id,
        }
    }
}

const PAGE_SIZE: i64 = 50;

/// What the request asks for: Live | All, the page (`before`: the last instance of the previous
/// one) and whether the page refreshes itself (`live=0` pauses it).
#[derive(Debug)]
struct ListParams {
    filter: InstanceFilter,
    before: Option<String>,
    live: bool,
}

impl Default for ListParams {
    fn default() -> Self {
        ListParams {
            filter: InstanceFilter::default(),
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
                "show" => params.filter = InstanceFilter::parse(&value),
                "before" if !value.is_empty() => params.before = Some(value.into_owned()),
                "live" => params.live = value != "0",
                _ => {}
            }
        }
        params
    }

    fn query(filter: InstanceFilter, before: Option<&str>, live: bool) -> String {
        let mut out = form_urlencoded::Serializer::new(String::new());
        out.append_pair("show", filter.as_str());
        if let Some(before) = before {
            out.append_pair("before", before);
        }
        if !live {
            out.append_pair("live", "0");
        }
        out.finish()
    }
}

/// The table with its pager: refreshed every 5 s by the page (the same page of the list).
pub struct InstancesList {
    rows: Vec<InstanceRowView>,
    live: bool,
    pause_href: String,
    resume_href: String,
    table_href: String,
    empty: &'static str,
    newest_href: Option<String>,
    older_href: Option<String>,
}

#[derive(Template, WebTemplate)]
#[template(path = "ui/instances.html")]
pub struct InstancesPage {
    nav: Nav,
    heartbeat_secs: u64,
    expire_secs: u64,
    filters: Vec<FilterLink>,
    list: InstancesList,
}

#[derive(Template, WebTemplate)]
#[template(path = "ui/partials/instances_table.html")]
pub struct InstancesPartial {
    list: InstancesList,
}

async fn load(state: &AppState, params: &ListParams) -> Result<InstancesList, Error> {
    let now = Utc::now();
    let views = instances::list_view(
        &state.pool,
        params.filter,
        params.before.as_deref(),
        PAGE_SIZE,
    )
    .await?;
    let rows: Vec<InstanceRowView> = views
        .iter()
        .map(|view| InstanceRowView::new(view, now, &state.instance_id))
        .collect();
    let href = |path: &str, before: Option<&str>, live: bool| {
        format!("{path}?{}", ListParams::query(params.filter, before, live))
    };
    let older_href = (i64::try_from(rows.len()) == Ok(PAGE_SIZE))
        .then(|| rows.last())
        .flatten()
        .map(|last| href("/ui/instances", Some(&last.id), params.live));
    Ok(InstancesList {
        live: params.live,
        pause_href: href("/ui/instances", params.before.as_deref(), false),
        resume_href: href("/ui/instances", params.before.as_deref(), true),
        table_href: href("/ui/instances/table", params.before.as_deref(), true),
        empty: match params.filter {
            InstanceFilter::Live => "No live instances.",
            InstanceFilter::All => "No instances yet.",
        },
        newest_href: params
            .before
            .as_ref()
            .map(|_| href("/ui/instances", None, params.live)),
        older_href,
        rows,
    })
}

pub async fn instances_page(
    State(state): State<Arc<AppState>>,
    RawQuery(query): RawQuery,
) -> Response {
    let params = ListParams::parse(query.as_deref());
    match load(&state, &params).await {
        Ok(list) => InstancesPage {
            nav: Nav::new(&state, "instances").with_live(Live::new(
                list.live,
                5,
                &list.pause_href,
                &list.resume_href,
            )),
            heartbeat_secs: HEARTBEAT_INTERVAL.as_secs(),
            expire_secs: INSTANCE_EXPIRE.as_secs(),
            filters: InstanceFilter::ALL
                .into_iter()
                .map(|filter| FilterLink {
                    label: filter.label(),
                    href: format!(
                        "/ui/instances?{}",
                        ListParams::query(filter, None, params.live)
                    ),
                    current: filter == params.filter,
                })
                .collect(),
            list,
        }
        .into_response(),
        Err(err) => server_error(&err),
    }
}

/// The table alone, refreshed every 5 s by the page.
pub async fn instances_table(
    State(state): State<Arc<AppState>>,
    RawQuery(query): RawQuery,
) -> Response {
    let params = ListParams::parse(query.as_deref());
    match load(&state, &params).await {
        Ok(list) => InstancesPartial { list }.into_response(),
        Err(err) => server_error(&err),
    }
}
