use axum::{
    Json,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};

use super::ParamError;

/// RFC 9457 problem details, sent as `application/problem+json`.
///
/// Without `type` the problem is `about:blank`: `title` is then the status reason phrase.
#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct Problem {
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub kind: Option<&'static str>,
    pub title: &'static str,
    /// Advisory in RFC 9457, but handy for clients and logs.
    #[serde(serialize_with = "status_code")]
    #[schema(value_type = u16)]
    pub status: StatusCode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Every invalid parameter, for [`super::INVALID_PARAMS_TYPE`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub errors: Option<Vec<ParamError>>,
    /// To find the request in traces and logs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
}

impl Problem {
    /// An `about:blank` problem: the status and its reason phrase.
    pub fn new(status: StatusCode) -> Self {
        Self {
            kind: None,
            title: status.canonical_reason().unwrap_or_default(),
            status,
            detail: None,
            errors: None,
            trace_id: crate::otel::current_trace_id(),
        }
    }
}

fn status_code<S: serde::Serializer>(status: &StatusCode, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_u16(status.as_u16())
}

impl IntoResponse for Problem {
    fn into_response(self) -> Response {
        (
            self.status,
            [(header::CONTENT_TYPE, "application/problem+json")],
            Json(self),
        )
            .into_response()
    }
}
