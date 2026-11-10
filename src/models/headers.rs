//! The headers of a job: which ones of a request are forwarded to the target, and which
//! values the API and the UI show of them (the others are kept for the target only).

use std::collections::HashMap;

use http::HeaderMap;

/// Prefix of the request headers forwarded to the target, without the prefix.
pub const FORWARD_PREFIX: &str = "irisqo-forward-";

/// The body headers, forwarded without the prefix.
const BODY_HEADERS: [&str; 3] = ["content-type", "content-encoding", "content-language"];

/// The forwarded headers whose values the API and the UI show: they describe the request
/// and carry no secret. Any other value (`authorization`, an API key, a signature...) is
/// shown as [`HIDDEN`]: stored for the target, never given back.
const OPEN_HEADERS: [&str; 8] = [
    "content-type",
    "content-encoding",
    "content-language",
    "accept",
    "accept-encoding",
    "accept-language",
    "user-agent",
    "cache-control",
];

pub const HIDDEN: &str = "••••";

/// The value of a forwarded header as shown: itself for an [open](OPEN_HEADERS) one.
pub fn shown_value<'a>(name: &str, value: &'a str) -> &'a str {
    if is_hidden(name, value) {
        HIDDEN
    } else {
        value
    }
}

/// Serializes the forwarded headers as shown (`#[serde(serialize_with)]` on the job): a job
/// cannot be given back with the values meant for the target.
pub fn serialize_shown<S: serde::Serializer>(
    headers: &Option<HashMap<String, String>>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    match headers {
        Some(headers) => serializer.collect_map(
            headers
                .iter()
                .map(|(name, value)| (name, shown_value(name, value))),
        ),
        None => serializer.serialize_none(),
    }
}

fn is_hidden(name: &str, value: &str) -> bool {
    !value.is_empty()
        && !OPEN_HEADERS
            .iter()
            .any(|open| name.eq_ignore_ascii_case(open))
}

/// The headers forwarded to the target: the body ones (`content-type`, `content-encoding`,
/// `content-language`) and the `irisqo-forward-*` ones without the prefix
/// (`irisqo-forward-authorization: Bearer x` -> `authorization: Bearer x`).
/// Everything else (the caller's own `authorization`, `cookie`, proxy headers...) stays here.
pub fn forwarded_headers(headers: &HeaderMap) -> HashMap<String, String> {
    headers
        .iter()
        .filter_map(|(name, value)| {
            let name = name.as_str();
            let target = name
                .strip_prefix(FORWARD_PREFIX)
                .or_else(|| BODY_HEADERS.contains(&name).then_some(name))?;
            let value = value.to_str().ok()?;
            (!is_reserved_header(target)).then(|| (target.to_owned(), value.to_owned()))
        })
        .collect()
}

/// Headers a client cannot set on the target request (lowercase name): the HTTP client
/// sets the host and the body length, irisqo the trace context and the `irisqo-*` ones.
#[must_use = "an unchecked header is forwarded"]
pub fn is_reserved_header(name: &str) -> bool {
    matches!(
        name,
        "" | "host"
            | "content-length"
            | "transfer-encoding"
            | "connection"
            | "traceparent"
            | "tracestate"
    ) || name.starts_with("irisqo-")
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderValue;

    #[test]
    fn forwarded_headers_allowlist() {
        let mut headers = HeaderMap::new();
        for (name, value) in [
            ("content-type", "application/json"),
            ("authorization", "Bearer caller"),
            ("cookie", "session=caller"),
            ("x-custom", "dropped"),
            ("irisqo-forward-authorization", "Bearer target"),
            ("irisqo-forward-x-custom", "kept"),
            ("irisqo-forward-host", "evil.example"),
            ("irisqo-forward-irisqo-job-id", "spoofed"),
            ("irisqo-forward-", "empty"),
            (
                "traceparent",
                "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
            ),
        ] {
            headers.insert(
                http::HeaderName::from_static(name),
                HeaderValue::from_static(value),
            );
        }
        let forwarded = forwarded_headers(&headers);
        assert_eq!(
            forwarded,
            HashMap::from([
                ("content-type".into(), "application/json".into()),
                ("authorization".into(), "Bearer target".into()),
                ("x-custom".into(), "kept".into()),
            ])
        );
    }

    #[test]
    fn only_open_headers_are_shown() {
        assert_eq!(
            shown_value("Content-Type", "application/json"),
            "application/json"
        );
        assert_eq!(shown_value("accept", "text/plain"), "text/plain");
        for secret in ["authorization", "x-api-key", "x-signature", "x-anything"] {
            assert_eq!(shown_value(secret, "value"), HIDDEN, "{secret}");
        }
        assert_eq!(shown_value("x-empty", ""), "");
    }
}
