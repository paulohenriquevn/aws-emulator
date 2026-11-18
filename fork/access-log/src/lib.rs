// SPDX-License-Identifier: CC0-1.0

//! fork: one log line per request the emulator answers.
//!
//! A controller that runs against the emulator (the AWS Load Balancer Controller, External
//! Secrets) logs little about the AWS calls it makes, and the emulator logs none, so a call
//! that is refused, unimplemented or slow leaves no trace. [`AccessLog`] wraps the router
//! and records, at `debug` on the `access` target, what each request was and how it was
//! answered: method, host, path, content type, the SigV4 scope's service and Region, the
//! operation (the `X-Amz-Target` header, or the query protocol's `Action`), the status,
//! the error code of an error answer, and how long it took. When the ROUTER itself refused
//! the request (`UnknownService`, `AmbiguousService`), its message is logged too: it names
//! only the routing key and the services that claimed the request.
//!
//! Nothing that authenticates a caller or that a caller stored is logged: no header but
//! `Host` and `X-Amz-Target`, no query string (a presigned URL carries its signature there),
//! and of a body only the `Action` of a form-encoded request and the error code of an error
//! answer. Turn it on with `RUST_LOG=info,access=debug`.

use std::time::Instant;

use bytes::Bytes;
use emulator_server::EmulatorService;

/// The longest value of a request field that is logged, in bytes.
const MAX_FIELD_BYTES: usize = 128;

/// The router `S`, logging every request it answers.
#[derive(Clone)]
pub struct AccessLog<S> {
    inner: S,
}

impl<S> AccessLog<S> {
    pub fn new(inner: S) -> Self {
        Self { inner }
    }
}

impl<S: EmulatorService> EmulatorService for AccessLog<S> {
    async fn handle_request(&self, request: http::Request<Bytes>) -> http::Response<Bytes> {
        if !tracing::enabled!(target: "access", tracing::Level::DEBUG) {
            return self.inner.handle_request(request).await;
        }
        let started = Instant::now();
        let method = request.method().to_string();
        let host = header(&request, "host");
        let content_type = header(&request, "content-type");
        let path = bounded(request.uri().path());
        let scope = emulator_server::sigv4_credential_scope(request.headers());
        let operation = operation(&request);
        let response = self.inner.handle_request(request).await;
        let status = response.status().as_u16();
        let error = if status >= 400 { error_code(response.body()) } else { String::new() };
        let routing_refusal = routing_refusal_message(&error, response.body());
        tracing::debug!(
            target: "access",
            method = %method,
            host = %host,
            path = %path,
            content_type = %content_type,
            service = %scope.as_ref().map(|scope| scope.service.as_str()).unwrap_or(""),
            region = %scope.as_ref().map(|scope| scope.region.as_str()).unwrap_or(""),
            operation = %operation,
            status,
            error = %error,
            routing_refusal = %routing_refusal,
            elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            "request"
        );
        response
    }

    fn serving_at(&self, endpoint_url: &str) {
        self.inner.serving_at(endpoint_url);
    }
}

fn header(request: &http::Request<Bytes>, name: &str) -> String {
    request
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(bounded)
        .unwrap_or_default()
}

/// The operation a request names: its `X-Amz-Target` (JSON protocols), else the `Action`
/// of a form-encoded body (query protocols), else nothing (REST protocols, whose path and
/// method name it).
fn operation(request: &http::Request<Bytes>) -> String {
    let target = header(request, "x-amz-target");
    if !target.is_empty() {
        return target;
    }
    let form = request
        .headers()
        .get(http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("application/x-www-form-urlencoded"));
    if form {
        form_action(request.body()).unwrap_or_default()
    } else {
        String::new()
    }
}

/// The `Action` parameter of a form-encoded body.
pub fn form_action(body: &[u8]) -> Option<String> {
    let body = std::str::from_utf8(body).ok()?;
    body.split('&')
        .find_map(|pair| pair.strip_prefix("Action="))
        .filter(|action| action.bytes().all(|b| b.is_ascii_alphanumeric()))
        .map(bounded)
}

/// The error code of an error answer: `__type` of a JSON body (without its namespace),
/// or the first `<Code>` of an XML one.
pub fn error_code(body: &[u8]) -> String {
    if let Ok(value) = serde_json::from_slice::<serde_json::Value>(body) {
        let code = value
            .get("__type")
            .or_else(|| value.get("code"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        return bounded(code.rsplit('#').next().unwrap_or_default());
    }
    let text = String::from_utf8_lossy(body);
    text.split_once("<Code>")
        .and_then(|(_, rest)| rest.split_once("</Code>"))
        .map(|(code, _)| bounded(code))
        .unwrap_or_default()
}

/// The router's own message when it refused to route the request, else nothing. Only
/// these two codes are the router's, and their message carries no request content.
pub fn routing_refusal_message(error: &str, body: &[u8]) -> String {
    if error != "UnknownService" && error != "AmbiguousService" {
        return String::new();
    }
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|value| value.get("message").and_then(serde_json::Value::as_str).map(|m| bounded_to(m, 512)))
        .unwrap_or_default()
}

fn bounded(value: &str) -> String {
    bounded_to(value, MAX_FIELD_BYTES)
}

fn bounded_to(value: &str, max: usize) -> String {
    let mut end = value.len().min(max);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_action_of_a_form_body_is_its_operation() {
        assert_eq!(form_action(b"Action=DescribeSubnets&Version=2016-11-15&Filter.1.Name=vpc-id"), Some("DescribeSubnets".to_string()));
        assert_eq!(form_action(b"Version=2011-06-15&Action=GetCallerIdentity"), Some("GetCallerIdentity".to_string()));
    }

    #[test]
    fn a_body_without_a_plain_action_names_no_operation() {
        assert_eq!(form_action(b"Version=2016-11-15"), None);
        assert_eq!(form_action(b"Action=%3Cscript%3E"), None);
        assert_eq!(form_action(&[0xff, 0xfe]), None);
    }

    #[test]
    fn a_json_error_is_logged_by_its_type_without_namespace() {
        assert_eq!(error_code(br#"{"__type":"com.amazonaws.eksauth#AccessDeniedException","message":"m"}"#), "AccessDeniedException");
        assert_eq!(error_code(br#"{"__type":"ResourceNotFoundException"}"#), "ResourceNotFoundException");
    }

    #[test]
    fn an_xml_error_is_logged_by_its_code() {
        let body = b"<ErrorResponse><Error><Type>Sender</Type><Code>InvalidSubnetID.NotFound</Code><Message>m</Message></Error></ErrorResponse>";
        assert_eq!(error_code(body), "InvalidSubnetID.NotFound");
    }

    #[test]
    fn an_error_body_with_no_code_logs_none() {
        assert_eq!(error_code(b"not found"), "");
    }

    #[test]
    fn a_routing_refusal_logs_the_routers_message() {
        let body = br#"{"__type":"AmbiguousService","message":"2 emulated services all claim requests for '<unresolved>': sts, iam; the request is rejected"}"#;
        assert!(routing_refusal_message("AmbiguousService", body).starts_with("2 emulated services all claim"));
    }

    #[test]
    fn a_service_error_logs_no_message() {
        let body = br#"{"__type":"ValidationException","message":"value 'secret' is too long"}"#;
        assert_eq!(routing_refusal_message("ValidationException", body), "");
    }

    #[test]
    fn a_long_value_is_cut_on_a_character_boundary() {
        let long = "é".repeat(100);
        let cut = bounded(&long);
        assert!(cut.len() <= MAX_FIELD_BYTES && long.starts_with(&cut));
    }
}
