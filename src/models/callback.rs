use std::collections::HashMap;

use hyper::{Method, Uri};
use serde::{Deserialize, Serialize};

use super::{FlowKey, JobRetry};

/// The format of a callback: what it sends and how.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum CallbackMode {
    /// JSON as Standard Webhooks: `type`, `timestamp`, `data`; `webhook-*` headers.
    #[default]
    Webhook,
    /// The response of the job as it is, with `Irisqo-*` headers.
    Relay,
}

impl CallbackMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            CallbackMode::Webhook => "webhook",
            CallbackMode::Relay => "relay",
        }
    }
}

/// The type of an event that can make a callback: how a job ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub enum CallbackType {
    #[serde(rename = "irisqo.job.completed")]
    Completed,
    #[serde(rename = "irisqo.job.failed")]
    Failed,
    #[serde(rename = "irisqo.job.cancelled")]
    Cancelled,
}

impl CallbackType {
    pub const RULE: &str =
        "must list irisqo.job.completed, irisqo.job.failed or irisqo.job.cancelled";

    pub const fn as_str(self) -> &'static str {
        match self {
            CallbackType::Completed => "irisqo.job.completed",
            CallbackType::Failed => "irisqo.job.failed",
            CallbackType::Cancelled => "irisqo.job.cancelled",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        [Self::Completed, Self::Failed, Self::Cancelled]
            .into_iter()
            .find(|t| t.as_str() == value)
    }

    /// The `processed.status` of a job that ends this way.
    pub const fn status(self) -> &'static str {
        match self {
            CallbackType::Completed => "completed",
            CallbackType::Failed => "failed",
            CallbackType::Cancelled => "cancelled",
        }
    }
}

/// The request that irisqo makes when the job reaches its outcome: `meta.callback` of the job.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct JobCallback {
    #[serde(with = "http_serde::uri")]
    #[schema(value_type = String)]
    pub url: Uri,
    /// `POST` when absent; only `POST` and `PUT` pass the check of the input.
    #[serde(
        with = "http_serde::option::method",
        skip_serializing_if = "Option::is_none",
        default
    )]
    #[schema(value_type = Option<String>)]
    pub method: Option<Method>,
    #[serde(default)]
    pub mode: CallbackMode,
    /// The types that make a callback: the three outcomes for a `webhook`, `completed` for a
    /// `relay` when absent.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub types: Option<Vec<CallbackType>>,
    /// Kept as the headers of a job: the API shows the values of the open ones only.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub headers: Option<HashMap<String, String>>,
    /// The default of a callback when absent (see `features::callbacks`).
    #[serde(flatten)]
    #[schema(schema_with = crate::handlers::job_request::retry_schema)]
    pub retry: Option<JobRetry>,
    /// Milliseconds; the default of a callback when absent.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub timeout: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub flow_key: Option<FlowKey>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub priority: Option<i32>,
}

impl JobCallback {
    pub fn method(&self) -> Method {
        self.method.clone().unwrap_or(Method::POST)
    }

    /// The outcomes that make a callback: `types`, else every one for a webhook and a response
    /// for a relay.
    pub fn effective_types(&self) -> &[CallbackType] {
        match (&self.types, self.mode) {
            (Some(types), _) => types,
            (None, CallbackMode::Webhook) => &[
                CallbackType::Completed,
                CallbackType::Failed,
                CallbackType::Cancelled,
            ],
            (None, CallbackMode::Relay) => &[CallbackType::Completed],
        }
    }

    /// Whether an outcome of this type makes a callback.
    pub fn fires_on(&self, kind: CallbackType) -> bool {
        self.effective_types().contains(&kind)
    }
}
