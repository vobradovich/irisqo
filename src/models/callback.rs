use std::{collections::HashMap, num::NonZeroU32};

use hyper::{Method, Uri};
use serde::{Deserialize, Serialize};

use super::{
    FlowKey, JobRetry, MILLISECONDS_RULE, PRIORITY_RULE, ParamError, Params, job::parse_method,
};

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
        serialize_with = "http_serde::option::method::serialize",
        deserialize_with = "deserialize_method",
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
    /// The default of a callback when absent (see `features::callbacks`); `"none"` is no retries.
    #[serde(
        skip_serializing_if = "Option::is_none",
        default,
        deserialize_with = "JobRetry::deserialize_optional"
    )]
    pub retry: Option<JobRetry>,
    /// Milliseconds; the default of a callback when absent.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    #[schema(value_type = Option<u32>, minimum = 1)]
    pub timeout: Option<NonZeroU32>,
    /// An empty one is none.
    #[serde(
        skip_serializing_if = "Option::is_none",
        default,
        deserialize_with = "FlowKey::deserialize_optional"
    )]
    pub flow_key: Option<FlowKey>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub priority: Option<i32>,
}

/// The method of a callback in upper case, as [`parse_method`]; the error names the field.
fn deserialize_method<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Method>, D::Error> {
    Option::<String>::deserialize(deserializer)?
        .map(|method| {
            parse_method(&method).ok_or_else(|| {
                serde::de::Error::custom("callback.method: is not a valid HTTP method")
            })
        })
        .transpose()
}

impl JobCallback {
    /// The fields of a callback that come as text, one by one: the `_callback_*` parameters of
    /// `/to/` without their prefix.
    pub const PARAMS: [&str; 8] = [
        "url", "method", "mode", "types", "retry", "timeout", "flow_key", "priority",
    ];

    /// The callback that `params` (fields of [`Self::PARAMS`] and their values) and `headers`
    /// describe: `Ok(None)` when they say nothing (an empty `url` alone is nothing), every wrong
    /// field in the `Err` as `callback.<field>`. What a callback must be besides its types (the
    /// target, the method, the headers) is checked with the job.
    pub fn from_params(
        params: &HashMap<String, String>,
        headers: HashMap<String, String>,
    ) -> Result<Option<Self>, Vec<ParamError>> {
        if headers.is_empty()
            && params
                .iter()
                .all(|(name, value)| name == "url" && value.is_empty())
        {
            return Ok(None);
        }
        let mut p = Params::new(params, "callback.");
        let url = if p.raw("url").is_some_and(|url| !url.is_empty()) {
            p.parse("url", "is not a valid URL", |v| Uri::try_from(v).ok())
        } else {
            p.error(
                "url",
                "is required by the other callback parameters and headers",
            );
            None
        };
        let method = p.parse("method", "is not a valid HTTP method", parse_method);
        let mode = p.parse("mode", "must be webhook or relay", |v| match v {
            "webhook" => Some(CallbackMode::Webhook),
            "relay" => Some(CallbackMode::Relay),
            _ => None,
        });
        let types = p.parse("types", CallbackType::RULE, |v| {
            v.split(',')
                .map(|t| CallbackType::parse(t.trim()))
                .collect()
        });
        // `none` is a policy of its own: no retries, not the default of a callback
        let retry = p
            .parse("retry", JobRetry::RULE, |v| JobRetry::parse(v).ok())
            .flatten();
        let timeout = p.parse("timeout", MILLISECONDS_RULE, |v| v.parse().ok());
        let flow_key = p
            .parse("flow_key", FlowKey::RULE, |v| {
                FlowKey::parse_optional(v).ok()
            })
            .flatten();
        let priority = p.parse("priority", PRIORITY_RULE, |v| v.parse().ok());
        let errors = p.into_errors();
        match url {
            Some(url) if errors.is_empty() => Ok(Some(Self {
                url,
                method,
                mode: mode.unwrap_or_default(),
                types,
                headers: (!headers.is_empty()).then_some(headers),
                retry,
                timeout,
                flow_key,
                priority,
            })),
            _ => Err(errors),
        }
    }

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
