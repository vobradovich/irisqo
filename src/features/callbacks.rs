//! Callbacks: the request a job makes when it reaches its outcome. The callback is a job of its
//! own, built here from the outcome and written in the transaction that records the outcome
//! (see `results::processed_many` and `jobqueue::cancel`).

use std::collections::HashMap;

use bytes::Bytes;
use chrono::{DateTime, Utc};
use hyper::header::{HeaderName, HeaderValue};
use serde::Serialize;

use crate::{
    features::results::JobResult,
    models::{
        CallbackMode, CallbackType, HttpMeta, JobCreate, JobMeta, JobRetry, JobRow, JobSink,
        headers::is_reserved_callback_header, new_ulid,
    },
};

/// The headers of a response that a `relay` sends on: the body ones.
const RELAYED: [&str; 3] = ["content-type", "content-encoding", "content-language"];

/// The retry policy of a callback without `callback_retry`: retried hard and for long (about
/// three days, with jitter), as Standard Webhooks asks. Not the job's.
pub const DEFAULT_RETRY: JobRetry = JobRetry::Exponential {
    count: 10,
    delay: 300,
    max_delay: Some(86_400),
};

/// The timeout of a callback without `callback_timeout`, in milliseconds.
pub const DEFAULT_TIMEOUT: u32 = 30_000;

/// How a job ended, with all that its callback is made of: the job as it is, borrowed, and the
/// result of its last attempt.
#[derive(Debug, Clone)]
pub struct JobOutcome<'a> {
    pub id: i64,
    pub key: Option<&'a str>,
    pub schedule_id: Option<&'a str>,
    pub meta: &'a JobMeta,
    /// The last attempt, from 0.
    pub attempt: i32,
    pub result: &'a JobResult,
    /// The `traceparent` of that attempt, which the callback goes on with; a cancelled job has
    /// none: the trace of the job itself goes on.
    pub trace: Option<String>,
}

impl<'a> JobOutcome<'a> {
    pub fn of(job: &'a JobRow, attempt: i32, result: &'a JobResult, trace: Option<String>) -> Self {
        JobOutcome {
            id: job.id,
            key: job.key.as_deref(),
            schedule_id: job.schedule_id.as_deref(),
            meta: &job.meta,
            attempt,
            result,
            trace,
        }
    }
}

/// The body of a `webhook` callback, as Standard Webhooks has it.
#[derive(Serialize, utoipa::ToSchema)]
pub struct CallbackWebhook<'a> {
    /// `irisqo.job.completed`, `irisqo.job.failed` or `irisqo.job.cancelled`.
    #[serde(rename = "type")]
    pub kind: &'static str,
    /// When the outcome was recorded.
    pub timestamp: DateTime<Utc>,
    pub data: WebhookData<'a>,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct WebhookData<'a> {
    pub job: WebhookJob<'a>,
    /// How the last attempt ended: what `GET /api/v1/jobs/{id}/result` gives.
    pub result: &'a JobResult,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct WebhookJob<'a> {
    pub id: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schedule_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// The last attempt, from 0.
    pub attempt: i32,
}

/// Why a header of a callback cannot be sent: a name that is not one of a header or is reserved
/// (see [`is_reserved_callback_header`]), a value that is not one of a header.
fn refused(name: &str, value: &str) -> Option<&'static str> {
    let Ok(parsed) = HeaderName::from_bytes(name.as_bytes()) else {
        return Some("is not the name of a header");
    };
    if is_reserved_callback_header(parsed.as_str()) {
        return Some("has a reserved name");
    }
    HeaderValue::from_str(value)
        .is_err()
        .then_some("has a value that is not allowed")
}

impl JobOutcome<'_> {
    /// Builds the callback of the outcome: `None` when it makes none (its type is not one the
    /// callback asks for, or a header of it cannot be sent). The callback job gets a key of its
    /// own, `cb_` and a new ULID.
    pub fn callback(&self) -> Option<JobCreate> {
        let callback = self.meta.callback.as_ref()?;
        let kind = self.result.callback_type();
        if !callback.fires_on(kind) {
            return None;
        }
        // the input refuses these; a job by hand (`enqueue_v1`) may have them: no callback, logged
        if let Some((name, why)) = callback
            .headers
            .iter()
            .flatten()
            .find_map(|(name, value)| refused(name, value).map(|why| (name, why)))
        {
            tracing::warn!(job_id = self.id, "no callback: its header `{name}` {why}");
            return None;
        }
        let (mut headers, body) = match callback.mode {
            CallbackMode::Relay => self.relay(kind),
            CallbackMode::Webhook => self.webhook(kind)?,
        };
        // the headers of the callback win over the ones taken from the response
        headers.extend(
            callback
                .headers
                .iter()
                .flatten()
                .map(|(name, value)| (name.to_ascii_lowercase(), value.clone())),
        );
        let meta = JobMeta {
            sink: JobSink::Http(HttpMeta {
                method: callback.method(),
                url: callback.url.clone(),
            }),
            retry: callback.retry.or(Some(DEFAULT_RETRY)),
            timeout: Some(
                callback
                    .timeout
                    .map_or(DEFAULT_TIMEOUT, std::num::NonZeroU32::get),
            ),
            flow_key: callback.flow_key.clone(),
            priority: callback.priority,
            traceparent: self.trace.clone().or_else(|| self.meta.traceparent.clone()),
            tracestate: self.meta.tracestate.clone(),
            callback: None,
            callback_as: Some(callback.mode),
        };
        Some(JobCreate {
            meta,
            headers: Some(headers),
            body,
            key: Some(format!("cb_{}", new_ulid())),
            // a callback runs as soon as it is made, and is not a schedule
            at: None,
            schedule: None,
        })
    }

    /// `relay`: the response as it is, with the headers irisqo says about the job.
    fn relay(&self, kind: CallbackType) -> (HashMap<String, String>, Bytes) {
        let mut headers = HashMap::new();
        // only a response is relayed: any other outcome sends the headers and no body
        let body = if kind == CallbackType::Completed {
            for name in RELAYED {
                if let Some(value) = self.result.headers.as_ref().and_then(|h| h.get(name)) {
                    headers.insert(name.to_owned(), value.clone());
                }
            }
            self.result.body.clone()
        } else {
            Bytes::new()
        };
        // the callback cannot carry these names (see `refused`), so none replaces them
        headers.insert("irisqo-parent-id".into(), self.id.to_string());
        // a key of any text may not be a header value: the receiver gets it from the job then
        if let Some(key) = self.key.filter(|k| HeaderValue::from_str(k).is_ok()) {
            headers.insert("irisqo-parent-key".into(), key.to_owned());
        }
        if let Some(status) = self.result.status_code() {
            headers.insert("irisqo-parent-status".into(), status.as_u16().to_string());
        }
        headers.insert("irisqo-event-type".into(), kind.as_str().into());
        (headers, body)
    }

    /// `webhook`: the JSON of Standard Webhooks; `None` when it cannot be made.
    fn webhook(&self, kind: CallbackType) -> Option<(HashMap<String, String>, Bytes)> {
        let (method, url) = match &self.meta.sink {
            JobSink::Http(http) => (Some(http.method.as_str()), Some(http.url.to_string())),
            JobSink::Noop => (None, None),
        };
        let webhook = CallbackWebhook {
            kind: kind.as_str(),
            timestamp: Utc::now(),
            data: WebhookData {
                job: WebhookJob {
                    id: self.id,
                    key: self.key,
                    schedule_id: self.schedule_id,
                    method,
                    url,
                    attempt: self.attempt,
                },
                result: self.result,
            },
        };
        let body = serde_json::to_vec(&webhook)
            .inspect_err(|err| {
                tracing::error!(
                    job_id = self.id,
                    "no callback: its body cannot be made: {err}"
                );
            })
            .ok()?;
        let headers = HashMap::from([("content-type".into(), "application/json".into())]);
        Some((headers, body.into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{features::results::JobResult, models::JobCallback};
    use bytes::Bytes;
    use hyper::{Method, StatusCode, Uri, Version};

    fn meta(mode: CallbackMode, types: Option<Vec<CallbackType>>) -> JobMeta {
        JobMeta {
            sink: JobSink::Http(HttpMeta {
                method: Method::POST,
                url: Uri::from_static("https://pay.example/charge"),
            }),
            timeout: Some(1500),
            callback: Some(JobCallback {
                url: Uri::from_static("https://me.example/hook"),
                method: None,
                mode,
                types,
                headers: Some(HashMap::from([("Authorization".into(), "Bearer x".into())])),
                retry: None,
                timeout: None,
                flow_key: None,
                priority: None,
            }),
            ..JobMeta::default()
        }
    }

    /// The callback of job 42 (key `order-42`) with `meta` that ended with `result`.
    fn callback(
        meta: &JobMeta,
        attempt: i32,
        result: &JobResult,
        trace: Option<String>,
    ) -> Option<JobCreate> {
        JobOutcome {
            id: 42,
            key: Some("order-42"),
            schedule_id: None,
            meta,
            attempt,
            result,
            trace,
        }
        .callback()
    }

    fn http(status: u16, body: &'static str) -> JobResult {
        JobResult::http(
            StatusCode::from_u16(status).unwrap(),
            Version::HTTP_11,
            Some(HashMap::from([
                ("content-type".into(), "application/json".into()),
                ("set-cookie".into(), "a=b".into()),
            ])),
            Bytes::from_static(body.as_bytes()),
        )
    }

    #[test]
    fn relay_sends_the_response_on_completed_by_default() {
        let meta = meta(CallbackMode::Relay, None);
        let spawned = callback(&meta, 0, &http(201, r#"{"id":7}"#), None).expect("completed");
        let headers = spawned.headers.as_ref().expect("headers");
        assert_eq!(spawned.body.as_ref(), br#"{"id":7}"#);
        assert_eq!(headers["content-type"], "application/json");
        assert_eq!(headers["irisqo-parent-id"], "42");
        assert_eq!(headers["irisqo-parent-key"], "order-42");
        assert_eq!(headers["irisqo-parent-status"], "201");
        assert_eq!(headers["irisqo-event-type"], "irisqo.job.completed");
        assert_eq!(headers["authorization"], "Bearer x");
        assert!(!headers.contains_key("set-cookie"));
        assert_eq!(spawned.meta.callback_as, Some(CallbackMode::Relay));
        // the key of the callback is its own, and the defaults are the callback's, not the job's
        assert!(spawned.key.as_deref().is_some_and(|k| k.starts_with("cb_")));
        assert_eq!(spawned.meta.timeout, Some(DEFAULT_TIMEOUT));
        assert_eq!(spawned.meta.retry, Some(DEFAULT_RETRY));
        assert_eq!(spawned.meta.callback, None);
        // by default nothing else makes a relay
        assert!(callback(&meta, 0, &http(503, "busy"), None).is_none());
        assert!(callback(&meta, 0, &JobResult::cancelled(), None).is_none());
    }

    #[test]
    fn relay_of_another_type_sends_the_headers_and_no_body() {
        let types = vec![CallbackType::Failed, CallbackType::Cancelled];
        let meta = meta(CallbackMode::Relay, Some(types));
        assert!(callback(&meta, 0, &http(200, "ok"), None).is_none());
        let spawned = callback(&meta, 1, &http(503, "busy"), None).expect("failed");
        let headers = spawned.headers.as_ref().expect("headers");
        assert!(spawned.body.is_empty());
        assert_eq!(headers["irisqo-event-type"], "irisqo.job.failed");
        assert_eq!(headers["irisqo-parent-status"], "503");
        assert!(!headers.contains_key("content-type"));
        let spawned = callback(&meta, 0, &JobResult::cancelled(), None).expect("cancelled");
        let headers = spawned.headers.as_ref().expect("headers");
        assert_eq!(headers["irisqo-event-type"], "irisqo.job.cancelled");
        assert!(!headers.contains_key("irisqo-parent-status"));
    }

    /// Input refuses these headers, but a job written by hand (`enqueue_v1`) may have them: it
    /// makes no callback.
    #[test]
    fn a_callback_with_a_header_that_cannot_be_sent_is_not_made() {
        let made = |name: &str, value: &str| {
            let mut meta = meta(CallbackMode::Relay, None);
            meta.callback.as_mut().expect("callback").headers =
                Some(HashMap::from([(name.to_owned(), value.to_owned())]));
            callback(&meta, 0, &http(200, "ok"), None).is_some()
        };
        for (name, value) in [
            ("Irisqo-Parent-Id", "999"),
            ("irisqo-event-type", "forged"),
            ("irisqo-job-id", "1"),
            ("irisqo-forward-host", "evil.example"),
            ("irisqo-forward-", "empty"),
            ("webhook-id", "1"),
            ("Host", "evil.example"),
            ("bad name", "1"),
            ("x-own", "bad\u{1}value"),
        ] {
            assert!(!made(name, value), "{name}");
        }
        for (name, value) in [
            ("x-own", "kept"),
            ("Authorization", "Bearer x"),
            ("Irisqo-Forward-Authorization", "Bearer for-next"),
        ] {
            assert!(made(name, value), "{name}");
        }
    }

    #[test]
    fn webhook_sends_the_standard_body_for_every_outcome() {
        let meta = meta(CallbackMode::Webhook, None);
        for (result, kind) in [
            (http(200, "ok"), "irisqo.job.completed"),
            (http(503, "busy"), "irisqo.job.failed"),
            (http(302, ""), "irisqo.job.failed"),
            (JobResult::cancelled(), "irisqo.job.cancelled"),
        ] {
            let spawned = callback(&meta, 2, &result, None).expect(kind);
            let headers = spawned.headers.as_ref().expect("headers");
            // the headers of the delivery are added when it is sent: none of Irisqo's here
            assert_eq!(headers["content-type"], "application/json");
            assert!(headers.keys().all(|name| !name.starts_with("irisqo-")));
            assert_eq!(spawned.meta.callback_as, Some(CallbackMode::Webhook));
            let body: serde_json::Value = serde_json::from_slice(&spawned.body).expect("json");
            assert_eq!(body["type"], kind);
            assert!(body["timestamp"].is_string());
            assert_eq!(body["data"]["job"]["id"], 42);
            assert_eq!(body["data"]["job"]["key"], "order-42");
            assert_eq!(body["data"]["job"]["attempt"], 2);
        }
        // the body of the response is base64
        let spawned = callback(&meta, 0, &http(200, "ok"), None).unwrap();
        let body: serde_json::Value = serde_json::from_slice(&spawned.body).unwrap();
        assert_eq!(body["data"]["result"]["body"], "b2s=");
    }

    #[test]
    fn a_callback_goes_on_the_trace_of_the_attempt_with_its_flags() {
        let meta = meta(CallbackMode::Webhook, None);
        let attempt = "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-00".to_owned();
        let spawned = callback(&meta, 0, &http(200, "ok"), Some(attempt.clone())).unwrap();
        // not sampled stays not sampled
        assert_eq!(spawned.meta.traceparent, Some(attempt));
        // with no trace of the attempt (a cancel) the one of the job goes on
        let mut of_the_job = self::meta(CallbackMode::Webhook, None);
        of_the_job.traceparent =
            Some("00-0af7651916cd43dd8448eb211c80319c-00f067aa0ba902b7-01".into());
        let spawned = callback(&of_the_job, 0, &http(200, "ok"), None).unwrap();
        assert_eq!(spawned.meta.traceparent, of_the_job.traceparent);
    }

    #[test]
    fn the_types_filter_a_webhook() {
        let meta = meta(CallbackMode::Webhook, Some(vec![CallbackType::Failed]));
        assert!(callback(&meta, 0, &http(200, "ok"), None).is_none());
        assert!(callback(&meta, 0, &http(500, ""), None).is_some());
    }
}
