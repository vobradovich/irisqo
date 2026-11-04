//! The "New job" form: its state, and the `/to/` request it stands for.
//!
//! The form never builds a job by itself: it composes the same `_*` query parameters
//! a `/to/` call would carry and goes through [`prepare_job`], so the API and the UI
//! accept exactly the same jobs, and the preview shows exactly what irisqo parses.

use std::collections::{BTreeMap, HashMap};

use axum::body::Bytes;
use chrono::NaiveDateTime;
use hyper::Method;
use url::form_urlencoded;

use crate::{
    handlers::http::{JobRequest, is_reserved_header, prepare_job, shown_value},
    models::{JobCreate, ParamError, WorkerOptions},
};

pub(super) const METHODS: [&str; 5] = ["GET", "POST", "PUT", "PATCH", "DELETE"];

/// `datetime-local` values, read as UTC (the browser does not send its time zone).
const DATETIME_FORMATS: [&str; 2] = ["%Y-%m-%dT%H:%M", "%Y-%m-%dT%H:%M:%S"];

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HeaderRow {
    pub name: String,
    pub value: String,
}

/// A job just created from the form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Created {
    pub id: i64,
    pub schedule_id: Option<String>,
}

/// The form as submitted: raw strings, so a rejected value is shown back as typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobForm {
    pub method: String,
    pub url: String,
    pub content_type: String,
    pub body: String,
    pub headers: Vec<HeaderRow>,
    /// `now` | `delay` | `at`
    pub when: String,
    pub delay: String,
    pub at: String,
    /// `once` | `interval` | `cron`
    pub repeat: String,
    pub interval: String,
    pub cron: String,
    pub until: String,
    /// `none` | `immediate` | `fixed` | `fibonacci`
    pub retry: String,
    pub retry_count: String,
    pub retry_delay: String,
    pub timeout: String,
    pub external_id: String,
    /// Field id -> message; `form` for errors of the whole form.
    pub errors: BTreeMap<&'static str, String>,
    pub created: Option<Created>,
    /// Shown as hints: `--timeout` and `--max-timeout`, ms.
    pub default_timeout: u32,
    pub max_timeout: u32,
}

impl Default for JobForm {
    fn default() -> Self {
        Self {
            method: "POST".into(),
            url: String::new(),
            content_type: "application/json".into(),
            body: String::new(),
            headers: Vec::new(),
            when: "now".into(),
            delay: String::new(),
            at: String::new(),
            repeat: "once".into(),
            interval: String::new(),
            cron: String::new(),
            until: String::new(),
            retry: "none".into(),
            retry_count: String::new(),
            retry_delay: String::new(),
            timeout: String::new(),
            external_id: String::new(),
            errors: BTreeMap::new(),
            created: None,
            default_timeout: 3000,
            max_timeout: 300_000,
        }
    }
}

/// `value` if it is one of `allowed`, the first allowed value otherwise.
fn one_of(value: String, allowed: &[&str]) -> String {
    if allowed.contains(&value.as_str()) {
        value
    } else {
        allowed.first().copied().unwrap_or_default().to_owned()
    }
}

impl JobForm {
    /// From `application/x-www-form-urlencoded` pairs (a POST body or a GET query).
    /// `op=add_header` / `op=remove_header&index=N` edit the header rows.
    pub fn from_pairs(pairs: &[u8]) -> Self {
        let mut form = JobForm::default();
        let mut op = String::new();
        let mut index = String::new();
        for (key, value) in form_urlencoded::parse(pairs) {
            let value = value.into_owned();
            match key.as_ref() {
                "method" => form.method = value,
                "url" => form.url = value,
                "content_type" => form.content_type = value,
                "body" => form.body = value,
                "header_name" => form.headers.push(HeaderRow {
                    name: value,
                    value: String::new(),
                }),
                "header_value" => {
                    if let Some(row) = form.headers.last_mut() {
                        row.value = value;
                    }
                }
                "when" => form.when = value,
                "delay" => form.delay = value,
                "at" => form.at = value,
                "repeat" => form.repeat = value,
                "interval" => form.interval = value,
                "cron" => form.cron = value,
                "until" => form.until = value,
                "retry" => form.retry = value,
                "retry_count" => form.retry_count = value,
                "retry_delay" => form.retry_delay = value,
                "timeout" => form.timeout = value,
                "external_id" => form.external_id = value,
                "op" => op = value,
                "index" => index = value,
                _ => {}
            }
        }
        form.method = one_of(form.method.to_uppercase(), &METHODS);
        form.when = one_of(form.when, &["now", "delay", "at"]);
        form.repeat = one_of(form.repeat, &["once", "interval", "cron"]);
        form.retry = one_of(form.retry, &["none", "immediate", "fixed", "fibonacci"]);
        // after the loop: `index` may come after `op`
        let index = index
            .parse::<usize>()
            .ok()
            .filter(|&i| i < form.headers.len());
        match (op.as_str(), index) {
            ("add_header", _) => form.headers.push(HeaderRow::default()),
            ("remove_header", Some(i)) => _ = form.headers.remove(i),
            _ => {}
        }
        form
    }

    /// The `_*` query parameters of the equivalent `/to/` request, by form field.
    /// Values that cannot even be converted (a date) are reported as field errors.
    fn job_params(&self) -> (Vec<(&'static str, String)>, BTreeMap<&'static str, String>) {
        let mut params = Vec::new();
        let mut errors = BTreeMap::new();
        let mut unix = |field: &'static str, value: &str| -> Option<String> {
            let parsed = DATETIME_FORMATS
                .iter()
                .find_map(|f| NaiveDateTime::parse_from_str(value.trim(), f).ok());
            match parsed {
                Some(dt) => Some(dt.and_utc().timestamp().to_string()),
                None => {
                    errors.insert(field, "must be a date and time".into());
                    None
                }
            }
        };
        let filled = |value: &str| (!value.trim().is_empty()).then(|| value.trim().to_owned());
        match self.when.as_str() {
            "delay" => params.push(("_delay", self.delay.trim().to_owned())),
            "at" => {
                if let Some(t) = unix("at", &self.at) {
                    params.push(("_delay_until", t));
                }
            }
            _ => {}
        }
        match self.repeat.as_str() {
            "interval" => params.push(("_interval", self.interval.trim().to_owned())),
            "cron" => params.push(("_cron", self.cron.trim().to_owned())),
            _ => {}
        }
        if self.repeat != "once"
            && let Some(t) = filled(&self.until).and_then(|until| unix("until", &until))
        {
            params.push(("_until", t));
        }
        match self.retry.as_str() {
            "immediate" => params.push(("_retry", self.retry_count.trim().to_owned())),
            kind @ ("fixed" | "fibonacci") => params.push((
                "_retry",
                format!(
                    "{}|{kind}|{}",
                    self.retry_count.trim(),
                    self.retry_delay.trim()
                ),
            )),
            _ => {}
        }
        if let Some(timeout) = filled(&self.timeout) {
            params.push(("_timeout", timeout));
        }
        if let Some(id) = filled(&self.external_id) {
            params.push(("_id", id));
        }
        (params, errors)
    }

    /// The headers for the target: content type and the header rows.
    fn target_headers(&self) -> (HashMap<String, String>, Option<String>) {
        let mut headers = HashMap::new();
        let mut error = None;
        let ct = self.content_type.trim();
        if !ct.is_empty() {
            headers.insert("content-type".to_owned(), ct.to_owned());
        }
        for row in &self.headers {
            let name = row.name.trim().to_ascii_lowercase();
            if name.is_empty() && row.value.trim().is_empty() {
                continue;
            }
            let valid = hyper::header::HeaderName::from_bytes(name.as_bytes()).is_ok()
                && hyper::header::HeaderValue::from_str(row.value.trim()).is_ok();
            if !valid || is_reserved_header(&name) {
                error = Some(format!("{name:?} cannot be sent"));
                continue;
            }
            headers.insert(name, row.value.trim().to_owned());
        }
        (headers, error)
    }

    /// Validates the form the way `/to/` validates a request and builds the job;
    /// the errors are keyed by form field.
    pub fn prepare(
        &self,
        options: &WorkerOptions,
        now_secs: i64,
    ) -> Result<JobCreate, BTreeMap<&'static str, String>> {
        let (params, mut errors) = self.job_params();
        let (headers, header_error) = self.target_headers();
        if let Some(message) = header_error {
            errors.insert("headers", message);
        }
        if self.url.trim().is_empty() {
            errors.insert("url", "is required".into());
        }
        let query = form_urlencoded::Serializer::new(String::new())
            .extend_pairs(&params)
            .finish();
        let request = JobRequest {
            method: Method::from_bytes(self.method.as_bytes()).unwrap_or(Method::POST),
            url: self.url.trim(),
            query: Some(&query),
            headers,
            body: Bytes::from(self.body.clone()),
        };
        match prepare_job(options, request, now_secs) {
            Ok(job) if errors.is_empty() => Ok(job),
            Ok(_) => Err(errors),
            Err(param_errors) => {
                for ParamError { parameter, detail } in param_errors {
                    errors.entry(field_of(parameter)).or_insert(detail);
                }
                Err(errors)
            }
        }
    }

    /// The equivalent API call, one parameter per line.
    pub fn preview(&self) -> String {
        let (params, _) = self.job_params();
        let url = if self.url.trim().is_empty() {
            "https://example.com/hook"
        } else {
            self.url.trim()
        };
        let mut out = format!("{} /to/{url}", self.method);
        for (i, (key, value)) in params.iter().enumerate() {
            out.push_str(if i == 0 { "\n  ?" } else { "\n  &" });
            out.push_str(&format!("{key}={value}"));
        }
        let (headers, _) = self.target_headers();
        let mut sent: Vec<_> = headers.iter().collect();
        sent.sort();
        for (name, value) in sent {
            let header = if name == "content-type" {
                "Content-Type".to_owned()
            } else {
                format!("Irisqo-Forward-{name}")
            };
            out.push_str(&format!("\n{header}: {}", shown_value(name, value)));
        }
        if !self.body.is_empty() {
            out.push_str("\n\n");
            out.push_str(&self.body);
        }
        out
    }

    /// The timeout hints from the worker options.
    #[must_use]
    pub fn with_limits(mut self, options: &WorkerOptions) -> Self {
        self.default_timeout = options.timeout;
        self.max_timeout = options.max_timeout;
        self
    }

    pub fn error(&self, field: &str) -> Option<&str> {
        self.errors.get(field).map(String::as_str)
    }
}

/// The form field of a `/to/` parameter.
fn field_of(param: &str) -> &'static str {
    match param {
        "_delay" => "delay",
        "_delay_until" => "at",
        "_interval" => "interval",
        "_cron" => "cron",
        "_until" => "until",
        "_retry" => "retry_count",
        "_timeout" => "timeout",
        "_id" => "external_id",
        _ => "url",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options() -> WorkerOptions {
        WorkerOptions {
            concurrency: 1,
            poll_interval: std::time::Duration::from_secs(1),
            prefetch: 1,
            timeout: 3000,
            max_timeout: 300_000,
            shutdown_timeout: std::time::Duration::from_secs(1),
            targets: std::sync::Arc::new(crate::models::TargetPolicy::allow_all()),
        }
    }

    #[test]
    fn header_rows_are_edited() {
        let form = JobForm::from_pairs(
            b"header_name=a&header_value=1&header_name=b&header_value=2&op=remove_header&index=0",
        );
        assert_eq!(
            form.headers,
            [HeaderRow {
                name: "b".into(),
                value: "2".into()
            }]
        );
        let form = JobForm::from_pairs(b"op=add_header");
        assert_eq!(form.headers.len(), 1);
    }

    #[test]
    fn unknown_choices_fall_back() {
        let form = JobForm::from_pairs(b"method=brew&when=later&retry=forever");
        assert_eq!(
            (
                form.method.as_str(),
                form.when.as_str(),
                form.retry.as_str()
            ),
            ("GET", "now", "none")
        );
    }

    #[test]
    fn errors_are_reported_by_field() {
        let form = JobForm::from_pairs(
            b"url=&when=delay&delay=soon&repeat=cron&cron=*/15+*+*+*&retry=fixed&retry_count=3&retry_delay=x&timeout=0&header_name=host&header_value=evil",
        );
        let errors = form
            .prepare(&options(), 1_700_000_000)
            .expect_err("invalid");
        let fields: Vec<_> = errors.keys().copied().collect();
        assert_eq!(
            fields,
            ["cron", "delay", "headers", "retry_count", "timeout", "url"]
        );
        assert!(errors["cron"].contains("got 4"));
    }

    #[test]
    fn valid_form_builds_the_job() {
        let form = JobForm::from_pairs(
            b"method=PUT&url=https%3A%2F%2Fexample.com%2Fhook%3Fa%3D1&when=at&at=2030-01-02T03%3A04&repeat=interval&interval=60&retry=fibonacci&retry_count=3&retry_delay=5&timeout=2000&external_id=order-1&header_name=Authorization&header_value=Bearer+x",
        );
        let job = form.prepare(&options(), 1_700_000_000).expect("valid");
        assert_eq!(job.at, Some(1_893_553_440));
        assert_eq!(job.meta.timeout, 2000);
        assert_eq!(job.external_id.as_deref(), Some("order-1"));
        let headers = job.headers.expect("headers");
        assert_eq!(
            headers.get("authorization").map(String::as_str),
            Some("Bearer x")
        );
        assert_eq!(
            headers.get("content-type").map(String::as_str),
            Some("application/json")
        );
        assert!(
            form.preview()
                .contains("Irisqo-Forward-authorization: ••••")
        );
        assert!(form.preview().contains("?_delay_until=1893553440"));
    }
}
