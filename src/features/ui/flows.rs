//! Flows: the list with its limits and backlog, and the forms to declare, change, pause,
//! resume and delete a flow.

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
    features::flows::{self, FlowCreate, FlowRequest, FlowRow, UndeclaredFlowRow},
    models::{AppState, Error, ParamError},
};

const PAGE_SIZE: i64 = 50;

pub struct FlowRowView {
    pub id: String,
    pub declared: bool,
    pub active: bool,
    /// `running`, `paused` or `dynamic` (used by jobs, never declared).
    pub state: &'static str,
    pub state_label: &'static str,
    pub concurrency: i32,
    /// `5 / s`, empty without a rate limit.
    pub rate: String,
    /// Only the names without a declaration are counted.
    pub counted: bool,
    pub ready: i64,
    pub processing: i64,
}

/// Which flows the list shows.
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

/// Where a page starts: after a name of the declared flows or of the others.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Part {
    Declared,
    Undeclared,
}

impl Part {
    const ALL: [Part; 2] = [Part::Declared, Part::Undeclared];

    const fn as_str(self) -> &'static str {
        match self {
            Part::Declared => "declared",
            Part::Undeclared => "undeclared",
        }
    }
}

#[derive(Debug)]
struct ListParams {
    show: Show,
    live: bool,
    /// Where the page starts: the part of the list and the last name before the page. The first
    /// page when absent.
    cursor: Option<(Part, Option<String>)>,
}

impl ListParams {
    fn parse(query: Option<&str>) -> Self {
        let (mut show, mut live, mut part, mut after) = (Show::default(), true, None, None);
        for (key, value) in form_urlencoded::parse(query.unwrap_or_default().as_bytes()) {
            match key.as_ref() {
                "show" => show = Show::parse(&value),
                "live" => live = value != "0",
                "part" => part = Part::ALL.into_iter().find(|p| p.as_str() == value),
                "after" if !value.is_empty() => after = Some(value.into_owned()),
                _ => {}
            }
        }
        // a name without a part is one of the declared flows
        let part = part.or_else(|| after.is_some().then_some(Part::Declared));
        ListParams {
            show,
            live,
            cursor: part.map(|part| (part, after)),
        }
    }

    fn cursor(&self) -> Option<(Part, Option<&str>)> {
        let (part, after) = self.cursor.as_ref()?;
        Some((*part, after.as_deref()))
    }

    fn query(&self, show: Show, live: bool, cursor: Option<(Part, Option<&str>)>) -> String {
        let mut out = form_urlencoded::Serializer::new(String::new());
        out.append_pair("show", show.as_str());
        if !live {
            out.append_pair("live", "0");
        }
        if let Some((part, after)) = cursor {
            out.append_pair("part", part.as_str());
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

impl FlowRowView {
    fn declared(row: &FlowRow) -> Self {
        let (state, state_label) = if row.paused {
            ("paused", "Paused")
        } else {
            ("running", "Active")
        };
        FlowRowView {
            id: row.flow_id.clone(),
            declared: true,
            active: !row.paused,
            state,
            state_label,
            concurrency: row.concurrency,
            rate: row
                .rate
                .map(|rate| format!("{rate} per {}", period(i64::from(row.rate_period))))
                .unwrap_or_default(),
            counted: false,
            ready: 0,
            processing: 0,
        }
    }

    fn undeclared(row: &UndeclaredFlowRow) -> Self {
        FlowRowView {
            id: row.flow_id.clone(),
            declared: false,
            active: true,
            state: "dynamic",
            state_label: "Undeclared",
            concurrency: 1,
            rate: String::new(),
            counted: true,
            ready: row.ready,
            processing: row.processing,
        }
    }
}

/// The form of a flow: the fields as typed, and what is wrong with them.
#[derive(Debug, Clone)]
pub struct FlowForm {
    /// The flow being changed; `None` for a new one.
    pub editing: Option<String>,
    pub flow_key: String,
    pub concurrency: String,
    pub rate: String,
    pub rate_period: String,
    pub errors: BTreeMap<&'static str, String>,
}

impl FlowForm {
    fn new() -> Self {
        FlowForm {
            editing: None,
            flow_key: String::new(),
            concurrency: "1".into(),
            rate: String::new(),
            rate_period: "1".into(),
            errors: BTreeMap::new(),
        }
    }

    fn from_row(row: &FlowRow) -> Self {
        FlowForm {
            editing: Some(row.flow_id.clone()),
            flow_key: row.flow_id.clone(),
            concurrency: row.concurrency.to_string(),
            rate: row.rate.map(|r| r.to_string()).unwrap_or_default(),
            rate_period: row.rate_period.to_string(),
            errors: BTreeMap::new(),
        }
    }

    fn from_pairs(editing: Option<String>, body: &[u8]) -> Self {
        let mut form = FlowForm {
            editing,
            flow_key: String::new(),
            concurrency: String::new(),
            rate: String::new(),
            rate_period: String::new(),
            errors: BTreeMap::new(),
        };
        for (key, value) in form_urlencoded::parse(body) {
            let value = value.trim().to_owned();
            match key.as_ref() {
                "flow_key" => form.flow_key = value,
                "concurrency" => form.concurrency = value,
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
    fn checked(&self) -> Result<FlowCreate, BTreeMap<&'static str, String>> {
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
        let request = FlowRequest {
            concurrency: number(
                &self.concurrency,
                "concurrency",
                "a whole number",
                &mut errors,
            ),
            rate: number(&self.rate, "rate", "a whole number", &mut errors),
            rate_period: number(
                &self.rate_period,
                "rate_period",
                "a whole number of seconds",
                &mut errors,
            ),
        };
        match request.validate(self.editing.as_deref().unwrap_or(&self.flow_key)) {
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
pub struct FlowsList {
    rows: Vec<FlowRowView>,
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
    rows: Vec<FlowRowView>,
    next: Option<(Part, Option<String>)>,
}

/// The page the request asks for: the declared flows (straight from `flows`) first, then the
/// names without a declaration, each part by name.
async fn load_page(state: &AppState, params: &ListParams) -> Result<Page, Error> {
    let pool = &state.pool;
    let cursor = params.cursor();
    let start = cursor.map_or(
        match params.show {
            Show::Undeclared => Part::Undeclared,
            Show::Declared | Show::All => Part::Declared,
        },
        |(part, _)| part,
    );
    let after = cursor.and_then(|(_, after)| after);
    let size = PAGE_SIZE as usize;
    let mut rows = Vec::new();
    if start == Part::Declared && params.show != Show::Undeclared {
        let mut found = flows::db::list(pool, after, PAGE_SIZE + 1, 0).await?;
        let more = found.len() > size;
        found.truncate(size);
        rows.extend(found.iter().map(FlowRowView::declared));
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
    let mut found = flows::db::list_undeclared(pool, after, room as i64 + 1, 0).await?;
    let more = found.len() > room;
    found.truncate(room);
    rows.extend(found.iter().map(FlowRowView::undeclared));
    let next = more.then(|| {
        let last = rows.last().filter(|r| !r.declared).map(|r| r.id.clone());
        (Part::Undeclared, last)
    });
    Ok(Page { rows, next })
}

async fn load(state: &AppState, params: &ListParams) -> Result<FlowsList, Error> {
    let page = load_page(state, params).await?;
    let cursor = params.cursor();
    let href = |path: &str, show: Show, live: bool, cursor| {
        format!("{path}?{}", params.query(show, live, cursor))
    };
    Ok(FlowsList {
        live: params.live,
        pause_href: href("/ui/flows", params.show, false, cursor),
        resume_href: href("/ui/flows", params.show, true, cursor),
        filters: Show::ALL
            .into_iter()
            .map(|show| FilterLink {
                label: show.label(),
                href: href("/ui/flows", show, params.live, None),
                current: show == params.show,
            })
            .collect(),
        table_href: href("/ui/flows/table", params.show, true, cursor),
        empty: match params.show {
            Show::Declared => "No declared flows. Declare one with New flow.",
            Show::Undeclared => "No jobs wait in an undeclared flow.",
            Show::All => "No flows. Declare one with New flow, or give a job a flow.",
        },
        first_href: cursor
            .is_some()
            .then(|| href("/ui/flows", params.show, params.live, None)),
        next_href: page.next.as_ref().map(|(part, after)| {
            href(
                "/ui/flows",
                params.show,
                params.live,
                Some((*part, after.as_deref())),
            )
        }),
        rows: page.rows,
    })
}

#[derive(Template, WebTemplate)]
#[template(path = "ui/flows.html")]
pub struct FlowsPage {
    nav: Nav,
    list: FlowsList,
}

#[derive(Template, WebTemplate)]
#[template(path = "ui/partials/flows_table.html")]
pub struct FlowsPartial {
    list: FlowsList,
}

#[derive(Template, WebTemplate)]
#[template(path = "ui/flow_form.html")]
pub struct FlowFormPage {
    nav: Nav,
    form: FlowForm,
}

pub async fn flows_page(State(state): State<Arc<AppState>>, RawQuery(query): RawQuery) -> Response {
    let params = ListParams::parse(query.as_deref());
    match load(&state, &params).await {
        Ok(list) => FlowsPage {
            nav: Nav::new(&state, "flows"),
            list,
        }
        .into_response(),
        Err(err) => server_error(&err),
    }
}

/// The table alone, refreshed every 2 s by the page.
pub async fn flows_table(
    State(state): State<Arc<AppState>>,
    RawQuery(query): RawQuery,
) -> Response {
    let params = ListParams::parse(query.as_deref());
    match load(&state, &params).await {
        Ok(list) => FlowsPartial { list }.into_response(),
        Err(err) => server_error(&err),
    }
}

fn form_page(state: &AppState, form: FlowForm) -> FlowFormPage {
    FlowFormPage {
        nav: Nav::new(state, "flows"),
        form,
    }
}

/// The form for a new flow; `?flow_key=` fills the name (to declare one that jobs already use).
pub async fn flow_new(State(state): State<Arc<AppState>>, RawQuery(query): RawQuery) -> Response {
    let mut form = FlowForm::new();
    if let Some(flow) = form_urlencoded::parse(query.unwrap_or_default().as_bytes())
        .find_map(|(key, value)| (key == "flow_key").then(|| value.into_owned()))
    {
        form.flow_key = flow;
    }
    form_page(&state, form).into_response()
}

pub async fn flow_edit(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    match flows::db::get_by_id(&state.pool, &id).await {
        Ok(Some(row)) => form_page(&state, FlowForm::from_row(&row)).into_response(),
        Ok(None) => Redirect::to("/ui/flows").into_response(),
        Err(err) => server_error(&err),
    }
}

fn invalid(
    state: &AppState,
    mut form: FlowForm,
    errors: BTreeMap<&'static str, String>,
) -> Response {
    form.errors = errors;
    (StatusCode::UNPROCESSABLE_ENTITY, form_page(state, form)).into_response()
}

pub async fn flow_create(State(state): State<Arc<AppState>>, body: Bytes) -> Response {
    let form = FlowForm::from_pairs(None, &body);
    let create = match form.checked() {
        Ok(create) => create,
        Err(errors) => return invalid(&state, form, errors),
    };
    // a new flow must not replace the limits of one that is declared
    match flows::db::get_by_id(&state.pool, &form.flow_key).await {
        Ok(Some(_)) => {
            let errors = BTreeMap::from([("flow_key", "is declared already".to_owned())]);
            return invalid(&state, form, errors);
        }
        Ok(None) => {}
        Err(err) => return server_error(&err),
    }
    match flows::db::upsert(&state.pool, &create).await {
        Ok(_) => Redirect::to("/ui/flows").into_response(),
        Err(err) => server_error(&err),
    }
}

pub async fn flow_update(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    let form = FlowForm::from_pairs(Some(id), &body);
    let update = match form.checked() {
        Ok(update) => update,
        Err(errors) => return invalid(&state, form, errors),
    };
    match flows::db::upsert(&state.pool, &update).await {
        Ok(_) => Redirect::to("/ui/flows").into_response(),
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
        Action::Pause => flows::db::pause(&state.pool, id).await,
        Action::Resume => flows::db::resume(&state.pool, id).await,
        Action::Delete => flows::db::delete(&state.pool, id).await,
    };
    if let Err(err) = done {
        return server_error(&err);
    }
    if !is_htmx(headers) {
        return Redirect::to("/ui/flows").into_response();
    }
    match load(state, &ListParams::parse(None)).await {
        Ok(list) => FlowsPartial { list }.into_response(),
        Err(err) => server_error(&err),
    }
}

pub async fn flow_pause(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    act(&state, &id, &headers, Action::Pause).await
}

pub async fn flow_resume(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    act(&state, &id, &headers, Action::Resume).await
}

pub async fn flow_delete(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    act(&state, &id, &headers, Action::Delete).await
}
