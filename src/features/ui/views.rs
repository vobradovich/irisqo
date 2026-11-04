//! What the templates show: plain strings, formatted on the server.

use std::collections::HashMap;

use chrono::{DateTime, TimeZone, Utc};

use crate::{
    db::jobview::{JobStatus, JobView},
    features::{
        history::JobHistoryRow,
        results::{JobResult, JobResultType},
    },
    models::{JobProtocol, JobRetry, JobRow},
};

/// Secrets are not echoed back: header values whose name looks like a credential.
pub fn mask(name: &str, value: &str) -> String {
    let secret = ["auth", "token", "key", "secret", "cookie", "password"]
        .iter()
        .any(|s| name.to_ascii_lowercase().contains(s));
    if secret && !value.is_empty() {
        "••••".to_owned()
    } else {
        value.to_owned()
    }
}

/// `in 4 min`, `12 s ago`, `now`.
pub fn relative(delta_secs: i64) -> String {
    let abs = delta_secs.unsigned_abs();
    let unit = match abs {
        0 => return "now".into(),
        1..60 => format!("{abs} s"),
        60..3600 => format!("{} min", abs / 60),
        3600..86_400 => format!("{} h", abs / 3600),
        _ => format!("{} d", abs / 86_400),
    };
    if delta_secs > 0 {
        format!("in {unit}")
    } else {
        format!("{unit} ago")
    }
}

pub fn utc(at: DateTime<Utc>) -> String {
    at.format("%Y-%m-%d %H:%M:%S UTC").to_string()
}

fn utc_secs(secs: i64) -> String {
    Utc.timestamp_opt(secs, 0)
        .single()
        .map(utc)
        .unwrap_or_default()
}

/// `host·K7QX`: the host and the end of the instance ULID.
pub fn short_instance(instance_id: &str) -> String {
    instance_id
        .rsplit_once(':')
        .and_then(|(host, ulid)| Some((host, ulid.get(ulid.len().checked_sub(4)?..)?)))
        .map_or_else(
            || instance_id.to_owned(),
            |(host, end)| format!("{host}·{end}"),
        )
}

/// `01JC…K7Q` for a ULID.
pub fn short_id(id: &str) -> String {
    let count = id.chars().count();
    if count <= 8 {
        return id.to_owned();
    }
    let head: String = id.chars().take(4).collect();
    let tail: String = id.chars().skip(count - 3).collect();
    format!("{head}…{tail}")
}

/// Attempts a job may get: the first one and its retries.
pub fn max_attempts(retry: &JobRetry) -> u32 {
    match retry {
        JobRetry::None => 1,
        JobRetry::Immediate { retry_count }
        | JobRetry::Fixed { retry_count, .. }
        | JobRetry::Fibonacci { retry_count, .. } => u32::from(*retry_count) + 1,
    }
}

pub fn retry_policy(retry: &JobRetry) -> String {
    match retry {
        JobRetry::None => "none".into(),
        JobRetry::Immediate { retry_count } => format!("immediate · {retry_count}"),
        JobRetry::Fixed {
            retry_count,
            retry_delay,
        } => format!("fixed · {retry_count} × {retry_delay} s"),
        JobRetry::Fibonacci {
            retry_count,
            retry_delay,
        } => format!("fibonacci · {retry_count} × {retry_delay} s"),
    }
}

pub fn status_label(status: &str) -> String {
    let mut chars = status.chars();
    chars
        .next()
        .map(|c| c.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

/// A job in a list.
#[derive(Debug, Clone)]
pub struct JobRowView {
    pub id: i64,
    pub status: JobStatus,
    pub status_label: String,
    pub method: String,
    pub url: String,
    /// `2 / 4`
    pub attempt: String,
    /// `in 4 min`, `waiting 3 s`, `started 2 s ago`...
    pub due: String,
    pub due_title: String,
    pub instance: String,
    pub instance_full: String,
    pub schedule: String,
    pub schedule_full: String,
}

impl JobRowView {
    pub fn new(job: &JobView, now: DateTime<Utc>) -> Self {
        let (method, url) = match &job.meta.protocol {
            JobProtocol::Http(http) => (http.method.to_string(), http.url.to_string()),
            JobProtocol::None => ("—".to_owned(), "no protocol".to_owned()),
        };
        let since = |at: DateTime<Utc>| relative((at - now).num_seconds());
        let (due, due_title) = match job.status {
            JobStatus::Scheduled | JobStatus::Retrying => {
                job.due_secs.map_or_else(Default::default, |secs| {
                    let delta = secs - now.timestamp();
                    let due = if delta <= 0 {
                        "due".to_owned()
                    } else {
                        relative(delta)
                    };
                    (due, utc_secs(secs))
                })
            }
            JobStatus::Enqueued => job.due_at.map_or_else(Default::default, |at| {
                (
                    format!(
                        "waiting {}",
                        relative((now - at).num_seconds()).trim_end_matches(" ago")
                    ),
                    utc(at),
                )
            }),
            JobStatus::Processing => job.lock_at.map_or_else(Default::default, |at| {
                (format!("started {}", since(at)), utc(at))
            }),
            JobStatus::Completed
            | JobStatus::Failed
            | JobStatus::Cancelled
            | JobStatus::Unknown => job
                .processed_at
                .map_or_else(Default::default, |at| (since(at), utc(at))),
        };
        let attempt = u32::try_from(job.retry).unwrap_or(0) + 1;
        let max = max_attempts(&job.meta.retry);
        // manual retries go past the policy
        let attempt = if attempt > max {
            attempt.to_string()
        } else {
            format!("{attempt} / {max}")
        };
        JobRowView {
            id: job.id,
            status_label: status_label(job.status.as_str()),
            status: job.status,
            method,
            url,
            attempt,
            due,
            due_title,
            instance: job
                .instance_id
                .as_deref()
                .map_or_else(|| "—".to_owned(), short_instance),
            instance_full: job.instance_id.clone().unwrap_or_default(),
            schedule: job
                .schedule_id
                .as_deref()
                .map_or_else(|| "—".to_owned(), short_id),
            schedule_full: job.schedule_id.clone().unwrap_or_default(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct HistoryView {
    pub at: String,
    pub at_title: String,
    pub status: String,
    pub label: String,
    pub retry: i32,
    pub instance: String,
    pub instance_full: String,
    pub message: String,
}

impl From<&JobHistoryRow> for HistoryView {
    fn from(row: &JobHistoryRow) -> Self {
        HistoryView {
            at: row.at.format("%H:%M:%S").to_string(),
            at_title: utc(row.at),
            status: row.status.clone(),
            label: status_label(&row.status),
            retry: row.retry,
            instance: short_instance(&row.instance_id),
            instance_full: row.instance_id.clone(),
            message: row.message.clone().unwrap_or_default(),
        }
    }
}

/// Up to 16 KiB of a body, as text.
fn body_text(body: &[u8]) -> String {
    const MAX: usize = 16 * 1024;
    let shown = body.get(..MAX).unwrap_or(body);
    let mut text = String::from_utf8_lossy(shown).into_owned();
    if body.len() > MAX {
        text.push_str("\n…");
    }
    text
}

fn sorted_headers(headers: Option<&HashMap<String, String>>) -> Vec<(String, String)> {
    let mut list: Vec<_> = headers
        .into_iter()
        .flatten()
        .map(|(k, v)| (k.clone(), mask(k, v)))
        .collect();
    list.sort();
    list
}

#[derive(Debug, Clone)]
pub struct ResultView {
    /// `503 Service Unavailable`, `Timeout`, `Error`...
    pub title: String,
    /// The status chip: completed | failed | cancelled
    pub status: String,
    pub detail: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
    pub body_size: usize,
    pub trace_id: Option<String>,
}

impl ResultView {
    pub fn new(result: &JobResult) -> Self {
        let (title, detail) = match &result.meta.result {
            JobResultType::Http(http) => (
                format!(
                    "{} {}",
                    http.status_code.as_u16(),
                    http.status_code.canonical_reason().unwrap_or_default()
                ),
                format!("{:?}", http.version),
            ),
            JobResultType::Timeout => ("Timeout".into(), String::new()),
            JobResultType::Cancelled => ("Cancelled".into(), String::new()),
            JobResultType::Error { error } => ("Error".into(), error.clone()),
            JobResultType::None => ("Done".into(), "no protocol".into()),
        };
        ResultView {
            title,
            status: result.processed_status().to_owned(),
            detail,
            headers: sorted_headers(result.headers.as_ref()),
            body: body_text(&result.body),
            body_size: result.body.len(),
            trace_id: result.meta.trace_id.clone(),
        }
    }
}

/// A job page.
#[derive(Debug, Clone)]
pub struct JobDetailView {
    pub row: JobRowView,
    pub created: String,
    pub retry_policy: String,
    pub timeout: String,
    pub external_id: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
    pub body_size: usize,
    pub result: Option<ResultView>,
    /// The link to the trace of the last attempt, when `--trace-url` is set.
    pub trace_href: Option<String>,
    pub history: Vec<HistoryView>,
    /// `(kind, text)`: `success` or `error`, after an action.
    pub notice: Option<(&'static str, String)>,
}

impl JobDetailView {
    pub fn new(
        view: &JobView,
        job: &JobRow,
        result: Option<&JobResult>,
        history: &[JobHistoryRow],
        trace_url: Option<&str>,
        now: DateTime<Utc>,
    ) -> Self {
        let result = result.map(ResultView::new);
        let trace_href = result
            .as_ref()
            .and_then(|r| r.trace_id.as_deref())
            .zip(trace_url)
            .map(|(id, template)| template.replace("{trace_id}", id));
        let body = job.body.as_deref().unwrap_or_default();
        JobDetailView {
            row: JobRowView::new(view, now),
            created: utc(view.created_at),
            retry_policy: retry_policy(&view.meta.retry),
            timeout: format!("{} ms", view.meta.timeout),
            external_id: view.external_id.clone().unwrap_or_else(|| "—".into()),
            headers: sorted_headers(job.headers.as_ref()),
            body: body_text(body),
            body_size: body.len(),
            result,
            trace_href,
            history: history.iter().map(HistoryView::from).collect(),
            notice: None,
        }
    }
}

#[test]
fn relative_times() {
    assert_eq!(relative(0), "now");
    assert_eq!(relative(42), "in 42 s");
    assert_eq!(relative(-240), "4 min ago");
    assert_eq!(relative(7_200), "in 2 h");
    assert_eq!(relative(-200_000), "2 d ago");
}

#[test]
fn short_ids() {
    assert_eq!(
        short_instance("api-7f9c:01JC8ZQ4V7M3N2K5R6T8W9XK7Q"),
        "api-7f9c·XK7Q"
    );
    assert_eq!(short_id("01JC8ZQ4V7M3N2K5R6T8W9XK7Q"), "01JC…K7Q");
    assert_eq!(
        max_attempts(&JobRetry::Fixed {
            retry_count: 3,
            retry_delay: 5
        }),
        4
    );
}
