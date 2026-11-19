//! The router answers console requests itself and hands everything else to the services.

use aws_emulator_fork_router::{build_router, InMemoryEngine};
use bytes::Bytes;
use emulator_server::EmulatorService;
use http::{header, Method, Request, StatusCode};
use std::sync::Arc;

/// Assembling the router needs a larger stack than a test thread has (see main.rs).
const ROUTER_STACK_SIZE: usize = 64 * 1024 * 1024;

fn answer(method: Method, uri: &'static str) -> http::Response<Bytes> {
    std::thread::Builder::new()
        .stack_size(ROUTER_STACK_SIZE)
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("a tokio runtime");
            let router = build_router(Arc::new(InMemoryEngine::new()));
            let request = Request::builder()
                .method(method)
                .uri(uri)
                .header(header::HOST, "localhost:9297")
                .body(Bytes::new())
                .expect("a valid request");
            runtime.block_on(router.handle_request(request))
        })
        .expect("spawn the router thread")
        .join()
        .expect("the router thread does not panic")
}

fn is_console_answer(response: &http::Response<Bytes>) -> bool {
    response
        .headers()
        .get(header::CONTENT_SECURITY_POLICY)
        .is_some_and(|policy| policy.to_str().unwrap_or_default().contains("frame-ancestors 'none'"))
}

#[test]
fn an_unsigned_console_request_is_answered_by_the_console() {
    let response = answer(Method::GET, "/_console/");
    let expected = if aws_emulator_fork_console::bundle().is_empty() {
        StatusCode::SERVICE_UNAVAILABLE
    } else {
        StatusCode::OK
    };
    assert_eq!(response.status(), expected);
    assert!(is_console_answer(&response));
}

#[test]
fn a_write_to_the_console_path_reaches_the_services() {
    let response = answer(Method::POST, "/_console/");
    assert!(!is_console_answer(&response), "status {}", response.status());
}
