//! Cross-site request forgery: a page on another site must not make the browser of a
//! user post to the UI (create, cancel or delete a job, pause a schedule).
//!
//! A request that changes something (not GET, HEAD or OPTIONS) passes when the browser
//! says it comes from the UI itself: `Sec-Fetch-Site: same-origin` (or `none`, typed by
//! the user), which a page cannot forge. A browser without it sends `Origin`, which must
//! be this host. A request with neither is not from a browser (curl, a script) and passes:
//! a browser always sends `Origin` with a form posted to another site.

use axum::{
    extract::Request,
    http::{HeaderMap, Method, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};

pub async fn same_origin(request: Request, next: Next) -> Response {
    let safe = matches!(
        *request.method(),
        Method::GET | Method::HEAD | Method::OPTIONS
    );
    if safe || from_this_site(request.headers()) {
        return next.run(request).await;
    }
    (StatusCode::FORBIDDEN, "Cross-site request refused.").into_response()
}

#[must_use = "an unchecked request is let through"]
fn from_this_site(headers: &HeaderMap) -> bool {
    let text = |name| headers.get(name).and_then(|value| value.to_str().ok());
    if let Some(site) = text(header::HeaderName::from_static("sec-fetch-site")) {
        return matches!(site, "same-origin" | "none");
    }
    let Some(origin) = text(header::ORIGIN) else {
        return true;
    };
    // behind a proxy the browser's host is in X-Forwarded-Host
    let host =
        text(header::HeaderName::from_static("x-forwarded-host")).or_else(|| text(header::HOST));
    let origin_host = origin
        .split_once("://")
        .map(|(_, authority)| authority.trim_end_matches('/'));
    origin_host
        .zip(host)
        .is_some_and(|(origin_host, host)| origin_host.eq_ignore_ascii_case(host))
}

#[cfg(test)]
mod tests {
    use super::*;

    type Headers = &'static [(&'static str, &'static str)];

    #[test]
    fn sites() {
        let cases: [(Headers, bool); 10] = [
            (&[("sec-fetch-site", "same-origin")], true),
            (&[("sec-fetch-site", "none")], true),
            (&[("sec-fetch-site", "cross-site")], false),
            (&[("sec-fetch-site", "same-site")], false),
            // Sec-Fetch-Site wins over a matching Origin
            (
                &[
                    ("sec-fetch-site", "cross-site"),
                    ("origin", "http://irisqo:8102"),
                    ("host", "irisqo:8102"),
                ],
                false,
            ),
            (
                &[("origin", "http://irisqo:8102"), ("host", "irisqo:8102")],
                true,
            ),
            (
                &[
                    ("origin", "https://queue.example.com"),
                    ("host", "10.0.0.5:8102"),
                    ("x-forwarded-host", "queue.example.com"),
                ],
                true,
            ),
            (
                &[("origin", "https://evil.example"), ("host", "irisqo:8102")],
                false,
            ),
            (&[("origin", "null"), ("host", "irisqo:8102")], false),
            // not a browser
            (&[("host", "irisqo:8102")], true),
        ];
        for (pairs, expected) in cases {
            let headers: HeaderMap = pairs
                .iter()
                .map(|&(name, value)| {
                    (
                        header::HeaderName::from_static(name),
                        header::HeaderValue::from_static(value),
                    )
                })
                .collect();
            assert_eq!(from_this_site(&headers), expected, "{pairs:?}");
        }
    }
}
