//! Queues: the list with its limits and backlog, and the forms to declare, change, pause,
//! resume and delete a queue.

use std::{collections::BTreeMap, sync::Arc};

use askama::Template;
use askama_web::WebTemplate;
use axum::{
    body::Bytes,
    extract::{Path, RawQuery, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Redirect, Response},
};
use url::form_urlencoded;

use super::{Nav, is_htmx, jobs::FilterLink, server_error};
use crate::{
    features::queues::{self, QueueCreate, QueueRequest, QueueRow, UndeclaredQueueRow},
    models::{AppState, Error, ParamError},
};

const PAGE_SIZE: i64 = 50;

pub struct QueueRowView {
    pub id: String,
    pub declared: bool,
    pub active: bool,
    /// `running`, `paused` or `dynamic` (used by jobs, never declared).
    pub state: &'static str,
    pub state_label: &'static str,
    pub parallelism: i32,
    pub order: &'static str,
    /// `5 / s`, empty without a rate limit.
    pub rate: String,
    /// Only the names without a declaration are counted.
    pub counted: bool,
    pub ready: i64,
    pub delayed: i64,
    pub processing: i64,
}

/// Which queues the list shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Show {
    #[default]
    Declared,
    Undeclared,
    /// The declared ones, then the others.
    All,
}

impl Show {
    const ALL: [Show; 3] = [Show::Declared, Show::Undeclared, Show::All];

    const fn as_str(self) -> &'static str {
        match self {
            Show::Declared => "declared",
            Show::Undeclared => "undeclared",
            Show::All => "all",
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Show::Declared => "Declared",
            Show::Undeclared => "Undeclared",
            Show::All => "All",
        }
    }

    fn parse(value: &str) -> Self {
        Self::ALL
            .into_iter()
            .find(|s| s.as_str() == value)
            .unwrap_or_default()
    }
}

/// Where a page starts: after a name of the declared queues or of the others.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Part {
    Declared,
    Undeclared,
}

#[derive(Debug, Default)]
struct ListParams {
    show: Show,
    live: bool,
    /// The part of the list `after` belongs to; the first one when absent.
    part: Option<Part>,
    after: Option<String>,
}

impl ListParams {
    fn parse(query: Option<&str>) -> Self {
        let mut params = ListParams {
            live: true,
            ..ListParams::default()
        };
        for (key, value) in form_urlencoded::parse(query.unwrap_or_default().as_bytes()) {
            match key.as_ref() {
                "show" => params.show = Show::parse(&value),
                "live" => params.live = value != "0",
                "part" => {
                    params.part = match value.as_ref() {
                        "declared" => Some(Part::Declared),
                        "undeclared" => Some(Part::Undeclared),
                        _ => None,
                    }
                }
                "after" if !value.is_empty() => params.after = Some(value.into_owned()),
                _ => {}
            }
        }
        params
    }

    fn query(&self, show: Show, live: bool, cursor: Option<(Part, Option<&str>)>) -> String {
        let mut out = form_urlencoded::Serializer::new(String::new());
        out.append_pair("show", show.as_str());
        if !live {
            out.append_pair("live", "0");
        }
        if let Some((part, after)) = cursor {
            out.append_pair(
                "part",
                match part {
                    Part::Declared => "declared",
                    Part::Undeclared => "undeclared",
                },
            );
            if let Some(after) = after {
                out.append_pair("after", after);
            }
        }
        out.finish()
    }
}

/// A number of seconds for a human: `1 s`, `30 s`, `5 min`, `2 h`.
fn period(secs: i64) -> String {
    match secs {
        s if s % 3600 == 0 => format!("{} h", s / 3600),
        s if s % 60 == 0 => format!("{} min", s / 60),
        s => format!("{s} s"),
    }
}

impl QueueRowView {
    fn declared(row: &QueueRow) -> Self {
        let (state, state_label) = if row.paused {
            ("paused", "Paused")
        } else {
            ("running", "Active")
        };
        QueueRowView {
            id: row.queue_id.clone(),
            declared: true,
            active: !row.paused,
            state,
            state_label,
            parallelism: row.parallelism,
            order: if row.ordered { "in order" } else { "any order" },
            rate: row
                .rate
                .map(|rate| format!("{rate} per {}", period(row.rate_period)))
                .unwrap_or_default(),
            counted: false,
            ready: 0,
            delayed: 0,
            processing: 0,
        }
    }

    fn undeclared(row: &UndeclaredQueueRow) -> Self {
        QueueRowView {
            id: row.queue_id.clone(),
            declared: false,
            active: true,
            state: "dynamic",
            state_label: "Undeclared",
            parallelism: 1,
            order: "in order",
            rate: String::new(),
            counted: true,
            ready: row.ready,
            delayed: row.delayed,
            processing: row.processing,
        }
    }
}

/// The form of a queue: the fields as typed, and what is wrong with them.
#[derive(Debug, Clone)]
pub struct QueueForm {
    /// The queue being changed; `None` for a new one.
    pub editing: Option<String>,
    pub queue: String,
    pub parallelism: String,
    pub ordered: bool,
    pub rate: String,
    pub rate_period: String,
    pub errors: BTreeMap<&'static str, String>,
}

impl QueueForm {
    fn new() -> Self {
        QueueForm {
            editing: None,
            queue: String::new(),
            parallelism: "1".into(),
            ordered: true,
            rate: String::new(),
            rate_period: "1".into(),
            errors: BTreeMap::new(),
        }
    }

    fn from_row(row: &QueueRow) -> Self {
        QueueForm {
            editing: Some(row.queue_id.clone()),
            queue: row.queue_id.clone(),
            parallelism: row.parallelism.to_string(),
            ordered: row.ordered,
            rate: row.rate.map(|r| r.to_string()).unwrap_or_default(),
            rate_period: row.rate_period.to_string(),
            errors: BTreeMap::new(),
        }
    }

    fn from_pairs(editing: Option<String>, body: &[u8]) -> Self {
        let mut form = QueueForm {
            editing,
            queue: String::new(),
            parallelism: String::new(),
            // a checkbox that is not checked is not sent
            ordered: false,
            rate: String::new(),
            rate_period: String::new(),
            errors: BTreeMap::new(),
        };
        for (key, value) in form_urlencoded::parse(body) {
            let value = value.trim().to_owned();
            match key.as_ref() {
                "queue" => form.queue = value,
                "parallelism" => form.parallelism = value,
                "ordered" => form.ordered = true,
                "rate" => form.rate = value,
                "rate_period" => form.rate_period = value,
                _ => {}
            }
        }
        form
    }

    pub fn error(&self, field: &str) -> Option<&str> {
        self.errors.get(field).map(String::as_str)
    }

    pub fn is_edit(&self) -> bool {
        self.editing.is_some()
    }

    /// The same checks as the API; an empty field is a missing one.
    fn checked(&self) -> Result<QueueCreate, BTreeMap<&'static str, String>> {
        fn number<T: std::str::FromStr>(
            value: &str,
            field: &'static str,
            what: &str,
            errors: &mut Vec<ParamError>,
        ) -> Option<T> {
            if value.is_empty() {
                return None;
            }
            value
                .parse()
                .map_err(|_| errors.push(ParamError::new(field, format!("must be {what}"))))
                .ok()
        }
        let mut errors = Vec::new();
        let request = QueueRequest {
            parallelism: number(
                &self.parallelism,
                "parallelism",
                "a whole number",
                &mut errors,
            ),
            ordered: Some(self.ordered),
            rate: number(&self.rate, "rate", "a whole number", &mut errors),
            rate_period: number(
                &self.rate_period,
                "rate_period",
                "a whole number of seconds",
                &mut errors,
            ),
        };
        match request.validate(self.editing.as_deref().unwrap_or(&self.queue)) {
            Ok(create) if errors.is_empty() => Ok(create),
            Ok(_) => Err(into_map(errors)),
            Err(more) => {
                errors.extend(more);
                Err(into_map(errors))
            }
        }
    }
}

fn into_map(errors: Vec<ParamError>) -> BTreeMap<&'static str, String> {
    errors
        .into_iter()
        .map(|e| (e.parameter, e.detail))
        .collect()
}

/// The table with its live switch, as the list of the jobs has it: refreshed every 2 s unless
/// the updates are paused (`?live=0`).
pub struct QueuesList {
    rows: Vec<QueueRowView>,
    live: bool,
    pause_href: String,
    resume_href: String,
    filters: Vec<FilterLink>,
    /// Refreshes this page of the list (the same filter and cursor).
    table_href: String,
    empty: &'static str,
    first_href: Option<String>,
    next_href: Option<String>,
}

/// A page of rows and where the next one starts.
struct Page {
    rows: Vec<QueueRowView>,
    next: Option<(Part, Option<String>)>,
}

/// The page the request asks for: the declared queues (straight from `queues`) first, then the
/// names without a declaration, each part by name.
async fn load_page(state: &AppState, params: &ListParams) -> Result<Page, Error> {
    let pool = &state.pool;
    let start = params.part.unwrap_or(match params.show {
        Show::Undeclared => Part::Undeclared,
        Show::Declared | Show::All => Part::Declared,
    });
    let after = params.after.as_deref();
    let size = PAGE_SIZE as usize;
    let mut rows = Vec::new();
    if start == Part::Declared && params.show != Show::Undeclared {
        let mut found = queues::db::list(pool, after, PAGE_SIZE + 1, 0).await?;
        let more = found.len() > size;
        found.truncate(size);
        rows.extend(found.iter().map(QueueRowView::declared));
        if more {
            return Ok(Page {
                next: rows.last().map(|r| (Part::Declared, Some(r.id.clone()))),
                rows,
            });
        }
        if params.show == Show::Declared {
            return Ok(Page { rows, next: None });
        }
    }
    // the room left on the page after the declared ones
    let room = size - rows.len();
    let after = after.filter(|_| start == Part::Undeclared);
    let mut found = queues::db::list_undeclared(pool, after, room as i64 + 1, 0).await?;
    let more = found.len() > room;
    found.truncate(room);
    rows.extend(found.iter().map(QueueRowView::undeclared));
    let next = more.then(|| {
        let last = rows.last().filter(|r| !r.declared).map(|r| r.id.clone());
        (Part::Undeclared, last)
    });
    Ok(Page { rows, next })
}

async fn load(state: &AppState, params: &ListParams) -> Result<QueuesList, Error> {
    let page = load_page(state, params).await?;
    let cursor = match (params.part, params.after.as_deref()) {
        (Some(part), after) => Some((part, after)),
        (None, Some(after)) => Some((Part::Declared, Some(after))),
        (None, None) => None,
    };
    let href = |path: &str, show: Show, live: bool, cursor| {
        format!("{path}?{}", params.query(show, live, cursor))
    };
    Ok(QueuesList {
        live: params.live,
        pause_href: href("/ui/queues", params.show, false, cursor),
        resume_href: href("/ui/queues", params.show, true, cursor),
        filters: Show::ALL
            .into_iter()
            .map(|show| FilterLink {
                label: show.label(),
                href: href("/ui/queues", show, params.live, None),
                current: show == params.show,
            })
            .collect(),
        table_href: href("/ui/queues/table", params.show, true, cursor),
        empty: match params.show {
            Show::Declared => "No declared queues. Declare one with New queue.",
            Show::Undeclared => "No jobs wait in an undeclared queue.",
            Show::All => "No queues. Declare one with New queue, or give a job a queue.",
        },
        first_href: cursor
            .is_some()
            .then(|| href("/ui/queues", params.show, params.live, None)),
        next_href: page.next.as_ref().map(|(part, after)| {
            href(
                "/ui/queues",
                params.show,
                params.live,
                Some((*part, after.as_deref())),
            )
        }),
        rows: page.rows,
    })
}

#[derive(Template, WebTemplate)]
#[template(path = "ui/queues.html")]
pub struct QueuesPage {
    nav: Nav,
    list: QueuesList,
}

#[derive(Template, WebTemplate)]
#[template(path = "ui/partials/queues_table.html")]
pub struct QueuesPartial {
    list: QueuesList,
}

#[derive(Template, WebTemplate)]
#[template(path = "ui/queue_form.html")]
pub struct QueueFormPage {
    nav: Nav,
    form: QueueForm,
}

pub async fn queues_page(
    State(state): State<Arc<AppState>>,
    RawQuery(query): RawQuery,
) -> Response {
    let params = ListParams::parse(query.as_deref());
    match load(&state, &params).await {
        Ok(list) => QueuesPage {
            nav: Nav::new(&state, "queues"),
            list,
        }
        .into_response(),
        Err(err) => server_error(&err),
    }
}

/// The table alone, refreshed every 2 s by the page.
pub async fn queues_table(
    State(state): State<Arc<AppState>>,
    RawQuery(query): RawQuery,
) -> Response {
    let params = ListParams::parse(query.as_deref());
    match load(&state, &params).await {
        Ok(list) => QueuesPartial { list }.into_response(),
        Err(err) => server_error(&err),
    }
}

fn form_page(state: &AppState, form: QueueForm) -> QueueFormPage {
    QueueFormPage {
        nav: Nav::new(state, "queues"),
        form,
    }
}

/// The form for a new queue; `?queue=` fills the name (to declare one that jobs already use).
pub async fn queue_new(State(state): State<Arc<AppState>>, RawQuery(query): RawQuery) -> Response {
    let mut form = QueueForm::new();
    if let Some(queue) = form_urlencoded::parse(query.unwrap_or_default().as_bytes())
        .find_map(|(key, value)| (key == "queue").then(|| value.into_owned()))
    {
        form.queue = queue;
    }
    form_page(&state, form).into_response()
}

pub async fn queue_edit(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    match queues::db::get_by_id(&state.pool, &id).await {
        Ok(Some(row)) => form_page(&state, QueueForm::from_row(&row)).into_response(),
        Ok(None) => Redirect::to("/ui/queues").into_response(),
        Err(err) => server_error(&err),
    }
}

fn invalid(
    state: &AppState,
    mut form: QueueForm,
    errors: BTreeMap<&'static str, String>,
) -> Response {
    form.errors = errors;
    (StatusCode::UNPROCESSABLE_ENTITY, form_page(state, form)).into_response()
}

pub async fn queue_create(State(state): State<Arc<AppState>>, body: Bytes) -> Response {
    let form = QueueForm::from_pairs(None, &body);
    let create = match form.checked() {
        Ok(create) => create,
        Err(errors) => return invalid(&state, form, errors),
    };
    // a new queue must not replace the limits of one that is declared
    match queues::db::get_by_id(&state.pool, &form.queue).await {
        Ok(Some(_)) => {
            let errors = BTreeMap::from([("queue", "is declared already".to_owned())]);
            return invalid(&state, form, errors);
        }
        Ok(None) => {}
        Err(err) => return server_error(&err),
    }
    match queues::db::upsert(&state.pool, &create).await {
        Ok(_) => Redirect::to("/ui/queues").into_response(),
        Err(err) => server_error(&err),
    }
}

pub async fn queue_update(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    let form = QueueForm::from_pairs(Some(id), &body);
    let update = match form.checked() {
        Ok(update) => update,
        Err(errors) => return invalid(&state, form, errors),
    };
    match queues::db::upsert(&state.pool, &update).await {
        Ok(_) => Redirect::to("/ui/queues").into_response(),
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
        Action::Pause => queues::db::pause(&state.pool, id).await,
        Action::Resume => queues::db::resume(&state.pool, id).await,
        Action::Delete => queues::db::delete(&state.pool, id).await,
    };
    if let Err(err) = done {
        return server_error(&err);
    }
    if !is_htmx(headers) {
        return Redirect::to("/ui/queues").into_response();
    }
    match load(state, &ListParams::parse(None)).await {
        Ok(list) => QueuesPartial { list }.into_response(),
        Err(err) => server_error(&err),
    }
}

pub async fn queue_pause(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    act(&state, &id, &headers, Action::Pause).await
}

pub async fn queue_resume(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    act(&state, &id, &headers, Action::Resume).await
}

pub async fn queue_delete(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    act(&state, &id, &headers, Action::Delete).await
}
