//! How the console answers requests, over a fixed table of files.

use aws_emulator_fork_console::{claims, respond_from};
use bytes::Bytes;
use http::{header, Method, Request, StatusCode};

const FILES: &[(&str, &[u8])] = &[
    ("index.html", b"<!doctype html><title>Console</title>"),
    ("assets/index-abc123.js", b"console.log(1)"),
    ("assets/index-abc123.css", b"body{}"),
    ("favicon.svg", b"<svg/>"),
];

fn request(method: Method, uri: &str) -> Request<Bytes> {
    Request::builder()
        .method(method)
        .uri(uri)
        .body(Bytes::new())
        .expect("a valid request")
}

fn get(uri: &str) -> http::Response<Bytes> {
    respond_from(FILES, &request(Method::GET, uri))
}

fn header_of<'a>(response: &'a http::Response<Bytes>, name: header::HeaderName) -> &'a str {
    response
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
}

#[test]
fn the_console_claims_reads_of_its_own_path_and_nothing_else() {
    assert!(claims(&request(Method::GET, "/_console")));
    assert!(claims(&request(Method::GET, "/_console/")));
    assert!(claims(&request(Method::HEAD, "/_console/assets/index-abc123.js")));
    assert!(!claims(&request(Method::POST, "/_console/")));
    assert!(!claims(&request(Method::GET, "/_consoles")));
    assert!(!claims(&request(Method::GET, "/my-bucket/_console/key")));
    assert!(!claims(&request(Method::GET, "/")));
}

#[test]
fn the_bare_console_path_redirects_to_the_folder_keeping_the_query() {
    let response = get("/_console?region=eu-west-1");
    assert_eq!(response.status(), StatusCode::PERMANENT_REDIRECT);
    assert_eq!(header_of(&response, header::LOCATION), "/_console/?region=eu-west-1");
}

#[test]
fn the_folder_answers_with_the_index_page() {
    let response = get("/_console/");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(header_of(&response, header::CONTENT_TYPE), "text/html; charset=utf-8");
    assert_eq!(header_of(&response, header::CACHE_CONTROL), "no-cache");
    assert_eq!(response.body().as_ref(), FILES[0].1);
}

#[test]
fn a_hashed_asset_is_served_with_its_type_and_cached_for_good() {
    let response = get("/_console/assets/index-abc123.js");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(header_of(&response, header::CONTENT_TYPE), "text/javascript; charset=utf-8");
    assert_eq!(
        header_of(&response, header::CACHE_CONTROL),
        "public, max-age=31536000, immutable"
    );
    assert_eq!(response.body().as_ref(), b"console.log(1)");
}

#[test]
fn a_client_side_route_answers_with_the_index_page() {
    let response = get("/_console/services/s3/buckets/my-bucket");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.body().as_ref(), FILES[0].1);
}

#[test]
fn a_missing_file_is_a_404_not_the_index_page() {
    let response = get("/_console/assets/missing.js");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[test]
fn a_head_request_carries_the_length_but_no_body() {
    let response = respond_from(FILES, &request(Method::HEAD, "/_console/favicon.svg"));
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(header_of(&response, header::CONTENT_LENGTH), "6");
    assert!(response.body().is_empty());
}

#[test]
fn every_answer_carries_the_security_headers() {
    for uri in ["/_console", "/_console/", "/_console/assets/missing.js"] {
        let response = get(uri);
        let policy = header_of(&response, header::CONTENT_SECURITY_POLICY);
        assert!(policy.contains("default-src 'self'"), "{uri}: {policy}");
        assert!(policy.contains("frame-ancestors 'none'"), "{uri}: {policy}");
        assert_eq!(header_of(&response, header::X_CONTENT_TYPE_OPTIONS), "nosniff", "{uri}");
        assert_eq!(header_of(&response, header::X_FRAME_OPTIONS), "DENY", "{uri}");
    }
}

#[test]
fn a_binary_built_without_the_console_says_how_to_build_it() {
    let response = respond_from(&[], &request(Method::GET, "/_console/"));
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = String::from_utf8_lossy(response.body());
    assert!(body.contains("npm ci && npm run build"), "{body}");
}
