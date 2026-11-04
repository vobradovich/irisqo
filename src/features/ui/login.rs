//! Login to the UI with an API token, when `--api-token` is set.
//!
//! The browser keeps a pass in an `HttpOnly` cookie, not the token: `<expires>.<hmac>`,
//! the HMAC-SHA256 of the expiry keyed by the token. Every page and htmx request carries the
//! cookie by itself, no script can read it, and nothing is stored on the server: a pass is
//! valid while its token is one of `--api-token` and it has not expired, so removing a token
//! logs out everyone who logged in with it.

use std::{fmt, sync::Arc, time::Duration};

use askama::Template;
use askama_web::WebTemplate;
use axum::{
    Form,
    extract::{Query, Request, State},
    http::{HeaderMap, Method, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Redirect, Response},
};
use chrono::Utc;
use hmac::{Hmac, KeyInit, Mac};
use serde::Deserialize;
use sha2::Sha256;
use url::form_urlencoded;

use super::{Nav, is_htmx};
use crate::models::{ApiTokens, AppState};

const COOKIE: &str = "irisqo_ui";
/// A pass lasts a week.
const PASS_SECS: i64 = 7 * 24 * 3600;
/// Where a login goes when it is not told where to go back to.
const HOME: &str = "/ui/jobs";

/// `<expires>.<hex of HMAC-SHA256(key = token, "irisqo-ui|" + expires as 8 big-endian bytes)>`
fn pass(token: &str, expires: i64) -> String {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(token.as_bytes()).expect("HMAC takes a key of any length");
    mac.update(b"irisqo-ui|");
    mac.update(&expires.to_be_bytes());
    format!("{expires}.{}", Hex(&mac.finalize().into_bytes()))
}

/// Bytes as lowercase hex, written straight into the formatter (no string per byte).
struct Hex<'a>(&'a [u8]);

impl fmt::Display for Hex<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.iter().try_for_each(|byte| write!(f, "{byte:02x}"))
    }
}

fn valid_pass(tokens: &ApiTokens, candidate: &str, now: i64) -> bool {
    let Some(expires) = candidate
        .split_once('.')
        .and_then(|(expires, _)| expires.parse::<i64>().ok())
    else {
        return false;
    };
    expires > now && tokens.accepts_derived(candidate, |token| pass(token, expires))
}

/// A cookie of the `Cookie` headers.
fn cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find_map(|(key, value)| (key == name).then_some(value))
}

/// `Secure` when the browser talks HTTPS (to a proxy in front): on plain HTTP a browser
/// would drop a `Secure` cookie, and the login would never stick.
fn secure(headers: &HeaderMap) -> &'static str {
    let https = headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|proto| proto.eq_ignore_ascii_case("https"));
    if https { "; Secure" } else { "" }
}

/// `SameSite=Lax`: sent when a link from elsewhere opens a page (`Strict` would ask for the
/// token again), not with a form posted from another site.
fn set_cookie(value: &str, max_age: i64, headers: &HeaderMap) -> String {
    format!(
        "{COOKIE}={value}; Path=/ui; Max-Age={max_age}; HttpOnly; SameSite=Lax{}",
        secure(headers)
    )
}

/// A page of the UI to go back to after the login: nothing else, so the login cannot
/// send anyone to another site.
fn safe_next(next: Option<&str>) -> &str {
    next.filter(|next| {
        let in_ui = *next == "/ui" || next.starts_with("/ui/") || next.starts_with("/ui?");
        in_ui
            && !next.starts_with("/ui/login")
            && next.chars().all(|c| c.is_ascii_graphic())
            && !next.contains('\\')
    })
    .unwrap_or(HOME)
}

fn login_location(next: Option<&str>) -> String {
    match safe_next(next) {
        HOME => "/ui/login".to_owned(),
        next => format!(
            "/ui/login?{}",
            form_urlencoded::Serializer::new(String::new())
                .append_pair("next", next)
                .finish()
        ),
    }
}

/// The login page, the assets and the icon are open; the rest of the UI asks for a pass.
fn is_open(path: &str) -> bool {
    path == "/ui/login" || path.starts_with("/ui/assets/") || path == "/favicon.ico"
}

pub async fn require_login(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Response {
    let open = match &state.api_tokens {
        None => true,
        Some(tokens) => {
            is_open(request.uri().path())
                || cookie(request.headers(), COOKIE)
                    .is_some_and(|pass| valid_pass(tokens, pass, Utc::now().timestamp()))
        }
    };
    if open {
        return next.run(request).await;
    }
    if is_htmx(request.headers()) {
        // a refresh or an action after the pass expired: htmx goes to the login page
        let back = request
            .headers()
            .get("hx-current-url")
            .and_then(|value| value.to_str().ok())
            .and_then(|url| url::Url::parse(url).ok())
            .map(|url| url[url::Position::BeforePath..url::Position::AfterQuery].to_owned());
        let location = login_location(back.as_deref());
        return (StatusCode::UNAUTHORIZED, [("hx-redirect", location)]).into_response();
    }
    let back = request
        .uri()
        .path_and_query()
        .filter(|_| request.method() == Method::GET)
        .map(|pq| pq.as_str());
    Redirect::to(&login_location(back)).into_response()
}

#[derive(Template, WebTemplate)]
#[template(path = "ui/login.html")]
pub struct LoginPage {
    nav: Nav,
    next: String,
    error: bool,
}

impl LoginPage {
    fn new(next: &str, error: bool) -> Self {
        LoginPage {
            nav: Nav {
                current: "login",
                logout: false,
            },
            next: next.to_owned(),
            error,
        }
    }
}

#[derive(Deserialize)]
pub struct LoginQuery {
    next: Option<String>,
}

pub async fn login_page(
    State(state): State<Arc<AppState>>,
    Query(query): Query<LoginQuery>,
) -> Response {
    let next = safe_next(query.next.as_deref());
    if state.api_tokens.is_none() {
        return Redirect::to(next).into_response();
    }
    LoginPage::new(next, false).into_response()
}

#[derive(Deserialize)]
pub struct LoginForm {
    token: String,
    next: Option<String>,
}

pub async fn login_submit(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<LoginForm>,
) -> Response {
    let next = safe_next(form.next.as_deref());
    let token = form.token.trim();
    let Some(tokens) = &state.api_tokens else {
        return Redirect::to(next).into_response();
    };
    if !tokens.accepts(token) {
        // a second per wrong guess
        tokio::time::sleep(Duration::from_secs(1)).await;
        return (StatusCode::UNAUTHORIZED, LoginPage::new(next, true)).into_response();
    }
    let value = pass(token, Utc::now().timestamp() + PASS_SECS);
    see_other(next, set_cookie(&value, PASS_SECS, &headers))
}

pub async fn logout(headers: HeaderMap) -> Response {
    see_other("/ui/login", set_cookie("", 0, &headers))
}

/// A `303` to `location` that sets the pass cookie.
fn see_other(location: &str, cookie: String) -> Response {
    let headers = [
        (header::LOCATION, location.to_owned()),
        (header::SET_COOKIE, cookie),
    ];
    (StatusCode::SEE_OTHER, headers).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_stays_in_the_ui() {
        assert_eq!(safe_next(Some("/ui/jobs/42?x=1")), "/ui/jobs/42?x=1");
        assert_eq!(safe_next(Some("/ui")), "/ui");
        for bad in [
            "https://evil.example/ui/",
            "//evil.example/ui/",
            "/uix",
            "/ui/login?next=/ui/jobs",
            "/ui/\\evil",
            "/ui/a b",
            "/ui/\r\nSet-Cookie: x",
        ] {
            assert_eq!(safe_next(Some(bad)), HOME, "{bad}");
        }
        assert_eq!(safe_next(None), HOME);
        assert_eq!(
            login_location(Some("/ui/jobs/42?x=1")),
            "/ui/login?next=%2Fui%2Fjobs%2F42%3Fx%3D1"
        );
        assert_eq!(login_location(Some("/ui/jobs")), "/ui/login");
    }

    #[test]
    fn cookies() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            "a=1; irisqo_ui=12.ab; b=2".parse().expect("value"),
        );
        assert_eq!(cookie(&headers, COOKIE), Some("12.ab"));
        assert_eq!(cookie(&headers, "c"), None);
    }

    #[test]
    fn pass_is_signed_by_the_token() {
        let a = pass("0123456789abcdef", 1_700_000_000);
        assert!(a.starts_with("1700000000.") && a.len() == 11 + 64);
        assert_ne!(a, pass("fedcba9876543210", 1_700_000_000));
        assert_ne!(a, pass("0123456789abcdef", 1_700_000_001));
    }
}
