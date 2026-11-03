//! Static files of the UI, compiled into the binary: no CDN, one file to deploy.

use axum::{
    extract::Path,
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};

struct Asset {
    name: &'static str,
    content_type: &'static str,
    bytes: &'static [u8],
    /// Versioned name: cached forever. `app.css` is revalidated instead.
    immutable: bool,
}

const ASSETS: &[Asset] = &[
    Asset {
        name: "htmx-2.0.8.min.js",
        content_type: "text/javascript; charset=utf-8",
        bytes: include_bytes!("../../../assets/vendor/htmx-2.0.8.min.js"),
        immutable: true,
    },
    Asset {
        name: "pico-2.1.1.classless.min.css",
        content_type: "text/css; charset=utf-8",
        bytes: include_bytes!("../../../assets/vendor/pico-2.1.1.classless.min.css"),
        immutable: true,
    },
    Asset {
        name: "app.css",
        content_type: "text/css; charset=utf-8",
        bytes: include_bytes!("../../../assets/app.css"),
        immutable: false,
    },
];

pub async fn asset(Path(name): Path<String>) -> Response {
    let Some(asset) = ASSETS.iter().find(|a| a.name == name) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let cache = if asset.immutable {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    (
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static(asset.content_type),
            ),
            (header::CACHE_CONTROL, HeaderValue::from_static(cache)),
        ],
        asset.bytes,
    )
        .into_response()
}
