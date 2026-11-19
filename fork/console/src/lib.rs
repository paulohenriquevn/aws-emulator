#![forbid(unsafe_code)]
//! The read-only web console this fork serves at [`CONSOLE_PATH`].
//!
//! The console is a single-page app (`web/`) that calls the emulator's own AWS APIs
//! from the browser with the AWS SDK, signed with the emulator's keys. This crate only
//! serves its files: `build.rs` embeds `web/dist`, and [`respond`] answers a request
//! for one of them. The router mounts it ahead of the AWS services with [`claims`],
//! so a console request never reaches SigV4 authentication or service dispatch.

use bytes::Bytes;
use http::{header, HeaderValue, Method, Request, Response, StatusCode};

mod bundle {
    include!(concat!(env!("OUT_DIR"), "/bundle.rs"));
}

/// The path the console is served under. No S3 bucket can be named `_console`, so a
/// path-style S3 request never collides with it.
pub const CONSOLE_PATH: &str = "/_console";

/// The page every client-side route of the single-page app is answered with.
const INDEX: &str = "index.html";

/// Vite writes content-hashed file names under this folder, so they never change.
const HASHED_ASSETS: &str = "assets/";

/// What the page may load: only this origin, which is also where the AWS APIs it
/// calls live. Cloudscape sets inline `style` attributes, hence `'unsafe-inline'` for
/// styles (never for scripts).
const CONTENT_SECURITY_POLICY: &str = "default-src 'self'; script-src 'self'; \
     style-src 'self' 'unsafe-inline'; img-src 'self' data:; font-src 'self' data:; \
     connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'";

const NOT_BUILT: &str = "The console was not built into this emulator binary. \
     Run `npm ci && npm run build` in fork/console/web, then rebuild the emulator.\n";

/// The files of the console this binary was built with, as `(path, bytes)` with paths
/// relative to `web/dist`. Empty when the binary was built without the console.
pub fn bundle() -> &'static [(&'static str, &'static [u8])] {
    bundle::BUNDLE
}

/// Whether `request` is for the console: a `GET` or `HEAD` of [`CONSOLE_PATH`] or
/// anything under it.
pub fn claims(request: &Request<Bytes>) -> bool {
    let method = request.method();
    if method != Method::GET && method != Method::HEAD {
        return false;
    }
    let path = request.uri().path();
    path == CONSOLE_PATH
        || path
            .strip_prefix(CONSOLE_PATH)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// Answer a console request from the files embedded in this binary.
pub fn respond(request: &Request<Bytes>) -> Response<Bytes> {
    respond_from(bundle(), request)
}

/// Answer a console request from `files`, a table of `(path, bytes)`.
///
/// * [`CONSOLE_PATH`] itself redirects to `CONSOLE_PATH/`, so relative asset URLs
///   resolve under it.
/// * A path naming a file is answered with that file.
/// * Any other path whose last segment has no extension is a client-side route of
///   the app, answered with `index.html`.
/// * Anything else is a 404, and every path is a 503 when `files` is empty.
pub fn respond_from(files: &[(&'static str, &'static [u8])], request: &Request<Bytes>) -> Response<Bytes> {
    let uri = request.uri();
    let Some(relative) = uri.path().strip_prefix(CONSOLE_PATH) else {
        return text(StatusCode::NOT_FOUND, "Not a console path.\n");
    };
    if files.is_empty() {
        return text(StatusCode::SERVICE_UNAVAILABLE, NOT_BUILT);
    }
    if relative.is_empty() {
        let location = match uri.query() {
            Some(query) => format!("{CONSOLE_PATH}/?{query}"),
            None => format!("{CONSOLE_PATH}/"),
        };
        return secured(Response::builder().status(StatusCode::PERMANENT_REDIRECT))
            .header(header::LOCATION, location)
            .body(Bytes::new())
            .expect("a redirect with valid headers");
    }
    let relative = relative.trim_start_matches('/');
    let wanted = if relative.is_empty() { INDEX } else { relative };
    let found = find(files, wanted).or_else(|| {
        let last_segment = wanted.rsplit('/').next().unwrap_or(wanted);
        (!last_segment.contains('.'))
            .then(|| find(files, INDEX))
            .flatten()
    });
    let Some((path, bytes)) = found else {
        return text(StatusCode::NOT_FOUND, "No such console file.\n");
    };
    let cache_control = if path.starts_with(HASHED_ASSETS) {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    let body = if request.method() == Method::HEAD {
        Bytes::new()
    } else {
        Bytes::from_static(bytes)
    };
    secured(Response::builder().status(StatusCode::OK))
        .header(header::CONTENT_TYPE, content_type(path))
        .header(header::CONTENT_LENGTH, bytes.len())
        .header(header::CACHE_CONTROL, cache_control)
        .body(body)
        .expect("a file response with valid headers")
}

fn find(files: &[(&'static str, &'static [u8])], path: &str) -> Option<(&'static str, &'static [u8])> {
    files
        .iter()
        .find(|(name, _)| *name == path)
        .map(|(name, bytes)| (*name, *bytes))
}

fn text(status: StatusCode, message: &'static str) -> Response<Bytes> {
    secured(Response::builder().status(status))
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(Bytes::from_static(message.as_bytes()))
        .expect("a text response with valid headers")
}

fn secured(builder: http::response::Builder) -> http::response::Builder {
    builder
        .header(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(CONTENT_SECURITY_POLICY),
        )
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
        .header(header::REFERRER_POLICY, "no-referrer")
        .header(header::X_FRAME_OPTIONS, "DENY")
}

/// The media type of a file the Vite build writes, by extension.
fn content_type(path: &str) -> &'static str {
    let extension = path.rsplit_once('.').map_or("", |(_, extension)| extension);
    match extension.to_ascii_lowercase().as_str() {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "ttf" => "font/ttf",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}
