//! Static files of the UI, compiled into the binary: no CDN, one file to deploy.

use axum::{
    extract::Path,
    http::{StatusCode, header},
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
        name: "htmx-4.0.1.min.js",
        content_type: "text/javascript; charset=utf-8",
        bytes: include_bytes!("../../../assets/vendor/htmx-4.0.1.min.js"),
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
    Asset {
        name: "logo.svg",
        content_type: "image/svg+xml",
        bytes: include_bytes!("../../../assets/logo.svg"),
        immutable: false,
    },
    Asset {
        name: "favicon-32.png",
        content_type: "image/png",
        bytes: include_bytes!("../../../assets/favicon-32.png"),
        immutable: false,
    },
    Asset {
        name: "apple-touch-icon.png",
        content_type: "image/png",
        bytes: include_bytes!("../../../assets/apple-touch-icon.png"),
        immutable: false,
    },
];

pub async fn asset(Path(name): Path<String>) -> Response {
    serve(&name)
}

/// `/favicon.ico`, asked for by browsers on any page, the API's too: the PNG icon.
pub async fn favicon() -> Response {
    serve("favicon-32.png")
}

fn serve(name: &str) -> Response {
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
            (header::CONTENT_TYPE, asset.content_type),
            (header::CACHE_CONTROL, cache),
        ],
        asset.bytes,
    )
        .into_response()
}
