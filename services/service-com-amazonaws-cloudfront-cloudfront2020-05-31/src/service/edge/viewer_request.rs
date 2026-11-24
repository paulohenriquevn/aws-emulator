// SPDX-License-Identifier: CC0-1.0

//! fork: the VIEWER-REQUEST function a cache behaviour associates, run at the edge.
//!
//! A behaviour's `FunctionAssociations` name the CloudFront Functions that run for it, each
//! for one `EventType`. The one of type `viewer-request` runs once the behaviour is selected
//! and its `AllowedMethods` admit the request, and before the edge cache is read: it is
//! handed the event object CloudFront builds for the request ([`event`]) and runs the code
//! the function's `LIVE` stage holds — "you must publish a function to the LIVE stage to
//! associate it with a cache behavior" — in the runtime `crate::state::run_function` bounds.
//! What it returns decides the rest ([`answer`]):
//!
//! * a RESPONSE object (one carrying `statusCode`) is served to the viewer as it stands, and
//!   the origin is never asked;
//! * a REQUEST object goes on to the cache and the origin with its changes: "When a function
//!   changes the `uri` value, it changes the object that the viewer is requesting" but
//!   "doesn't change the cache behavior for the request", so the behaviour selected for the
//!   viewer's own path still applies. The method is "the only read-only field";
//! * a function that RAISES, that is stopped by a bound of the runtime, or whose answer is
//!   not an event object CloudFront can use, is a function execution error, answered `503`
//!   by CloudFront ([`FUNCTION_ERROR_STATUS`]).
//!
//! # The origin a function names
//!
//! `cf.updateRequestOrigin` changes the origin of THIS request ([`OriginUpdate`]): "Any
//! settings that you don't specify in the `updateRequestOrigin()` will inherit the same
//! settings from the existing origin's configuration." This emulator applies `domainName`,
//! `hostHeader`, `originPath` and `customHeaders`; a function that names any other member
//! is answered as a function execution error, with the member logged, rather than sent to an
//! origin the function did not describe.
//!
//! The origin request is made through the same in-process transport as any other, which
//! routes a request by its `Host`. So the request is addressed to `domainName` — which is
//! what reaches a load balancer, a bucket or any endpoint this emulator serves at that name
//! — and, when `hostHeader` differs from it, carries that host in `X-Forwarded-Host`, which
//! is where this emulator's Application Load Balancer reads the host a client addressed (and
//! passes it on to its targets as their `Host`). An origin served by any other service sees
//! `domainName` as the `Host`.

use aws_smithy_runtime_api::http::Headers;
use serde_json::{json, Map, Value};

/// `FunctionAssociation$EventType` of the function this module runs.
pub const VIEWER_REQUEST: &str = "viewer-request";

/// The version of the event object: "The current version is `1.0`."
const EVENT_VERSION: &str = "1.0";

/// What CloudFront answers a viewer whose request a function failed on: the function
/// execution error status.
pub const FUNCTION_ERROR_STATUS: u16 = 503;

/// The detail line of the page CloudFront serves for that failure.
pub const FUNCTION_ERROR_DETAIL: &str =
    "The CloudFront function associated with this distribution could not run, or returned \
     an event object CloudFront cannot use.";

/// `X-Cache` of a response a function generated rather than the cache or an origin.
pub const FUNCTION_GENERATED_RESPONSE: &str = "FunctionGeneratedResponse from cloudfront";

/// The header the host the function asked the origin to see travels in, beside the `Host`
/// the in-process transport routes by.
pub const FORWARDED_HOST: &str = "x-forwarded-host";

/// The `Cookie` header, which the event object carries as `cookies` rather than a header.
const COOKIE: &str = "cookie";

/// The longest domain name or host header an origin update may name: "The domain name can
/// be up to 253 characters."
const MAX_ORIGIN_NAME_LEN: usize = 253;

/// The longest origin path an origin update may name: "a maximum length of 255 characters".
const MAX_ORIGIN_PATH_LEN: usize = 255;

/// What CloudFront knows about the request when it builds the event object.
#[derive(Debug)]
pub struct ViewerRequest<'a> {
    pub distribution_domain_name: &'a str,
    pub distribution_id: &'a str,
    pub request_id: &'a str,
    pub viewer_ip: &'a str,
    pub method: &'a str,
    /// The viewer path within the distribution.
    pub uri: &'a str,
    /// The raw query string, without its `?`.
    pub query: &'a str,
    pub headers: &'a Headers,
}

/// The event object a viewer-request function is handed, as JSON text.
///
/// Header names are lowercased ("which is always the case in the `event` object that
/// CloudFront Functions passes to your function in production"), the `Cookie` headers are
/// carried as `cookies`, and a name that occurs more than once carries every value in
/// `multiValue` with the first repeated in `value`. Query string values are carried as the
/// viewer sent them, without percent-decoding.
pub fn event(request: &ViewerRequest<'_>) -> String {
    let mut headers: Vec<(String, String)> = Vec::new();
    let mut cookies: Vec<(String, String)> = Vec::new();
    for (name, value) in request.headers.iter() {
        let name = name.to_ascii_lowercase();
        if name == COOKIE {
            cookies.extend(cookie_pairs(value));
        } else {
            headers.push((name, value.to_string()));
        }
    }
    let query = request.query.split('&').filter(|pair| !pair.is_empty()).map(|pair| {
        let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
        (name.to_string(), value.to_string())
    });
    json!({
        "version": EVENT_VERSION,
        "context": {
            "distributionDomainName": request.distribution_domain_name,
            "distributionId": request.distribution_id,
            "eventType": VIEWER_REQUEST,
            "requestId": request.request_id,
        },
        "viewer": { "ip": request.viewer_ip },
        "request": {
            "method": request.method,
            "uri": request.uri,
            "querystring": fields(query),
            "headers": fields(headers),
            "cookies": fields(cookies),
        },
    })
    .to_string()
}

/// The `name=value` pairs of one `Cookie` header.
fn cookie_pairs(header: &str) -> impl Iterator<Item = (String, String)> + '_ {
    header
        .split(';')
        .map(str::trim)
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
            (name.trim().to_string(), value.trim().to_string())
        })
}

/// The event-object shape of query strings, headers and cookies: one field per name, its
/// first value in `value`, and every value in `multiValue` when the name repeats.
fn fields(pairs: impl IntoIterator<Item = (String, String)>) -> Value {
    let mut grouped: Vec<(String, Vec<String>)> = Vec::new();
    for (name, value) in pairs {
        match grouped.iter_mut().find(|(held, _)| *held == name) {
            Some((_, values)) => values.push(value),
            None => grouped.push((name, vec![value])),
        }
    }
    let mut object = Map::new();
    for (name, values) in grouped {
        let mut field = Map::new();
        field.insert("value".to_string(), Value::String(values[0].clone()));
        if values.len() > 1 {
            let every = values.into_iter().map(|value| json!({ "value": value })).collect();
            field.insert("multiValue".to_string(), Value::Array(every));
        }
        object.insert(name, Value::Object(field));
    }
    Value::Object(object)
}

/// What a viewer-request function's answer means for the request.
#[derive(Debug, PartialEq, Eq)]
pub enum FunctionAnswer {
    /// Serve this to the viewer; the origin is never asked.
    Response(GeneratedResponse),
    /// Go on to the cache and the origin with this request.
    Request(ModifiedRequest),
}

/// A response a function generated.
#[derive(Debug, PartialEq, Eq)]
pub struct GeneratedResponse {
    pub status: u16,
    /// Every header, lowercased, `Set-Cookie` headers for the `cookies` included.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// The request a function returned.
#[derive(Debug, PartialEq, Eq)]
pub struct ModifiedRequest {
    /// The path, which begins with `/`.
    pub uri: String,
    /// The query string, without its `?`.
    pub query: String,
    /// Every header, lowercased, with the `cookies` as one `Cookie` header.
    pub headers: Vec<(String, String)>,
}

/// What the event object a function returned (`run_function`'s output, `{"request": ...}`
/// or `{"response": ...}`) means, or why CloudFront cannot use it.
pub fn answer(output: &str) -> Result<FunctionAnswer, String> {
    let parsed: Value = serde_json::from_str(output)
        .map_err(|error| format!("the function's output is not JSON: {error}"))?;
    if let Some(response) = parsed.get("response") {
        return generated_response(response).map(FunctionAnswer::Response);
    }
    let request = parsed
        .get("request")
        .ok_or("the function returned neither a request nor a response")?;
    modified_request(request).map(FunctionAnswer::Request)
}

fn generated_response(response: &Value) -> Result<GeneratedResponse, String> {
    let status = response
        .get("statusCode")
        .and_then(Value::as_u64)
        .and_then(|status| u16::try_from(status).ok())
        .filter(|status| (100..=599).contains(status))
        .ok_or("the returned response's statusCode is not an HTTP status code")?;
    let mut headers = values_of(response.get("headers"), "headers")?;
    for (name, value) in values_of(response.get("cookies"), "cookies")? {
        headers.push(("set-cookie".to_string(), format!("{name}={value}")));
    }
    let body = match response.get("body") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::String(text)) => text.clone().into_bytes(),
        Some(Value::Object(body)) => {
            let data = body
                .get("data")
                .and_then(Value::as_str)
                .ok_or("the returned response's body carries no data")?;
            match body.get("encoding").and_then(Value::as_str).unwrap_or("text") {
                "text" => data.as_bytes().to_vec(),
                "base64" => ::aws_smithy_types::base64::decode(data)
                    .map_err(|_| "the returned response's base64 body is not base64".to_string())?,
                other => return Err(format!("the returned response's body encoding {other} is neither text nor base64")),
            }
        }
        Some(_) => return Err("the returned response's body is neither a string nor an object".to_string()),
    };
    Ok(GeneratedResponse {
        status,
        headers,
        body,
    })
}

fn modified_request(request: &Value) -> Result<ModifiedRequest, String> {
    let uri = request
        .get("uri")
        .and_then(Value::as_str)
        .ok_or("the returned request carries no uri")?;
    if !uri.starts_with('/') {
        return Err(format!("the returned request's uri {uri} does not begin with a forward slash"));
    }
    let query = match request.get("querystring") {
        // "A function can return a query string value in addition to a query string object."
        Some(Value::String(query)) => query.trim_start_matches('?').to_string(),
        other => values_of(other, "querystring")?
            .into_iter()
            .map(|(name, value)| {
                if value.is_empty() {
                    format!("{}=", encoded(&name))
                } else {
                    format!("{}={}", encoded(&name), encoded(&value))
                }
            })
            .collect::<Vec<_>>()
            .join("&"),
    };
    let mut headers: Vec<(String, String)> = values_of(request.get("headers"), "headers")?
        .into_iter()
        .filter(|(name, _)| name != COOKIE)
        .collect();
    let cookies: Vec<String> = values_of(request.get("cookies"), "cookies")?
        .into_iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect();
    if !cookies.is_empty() {
        headers.push((COOKIE.to_string(), cookies.join("; ")));
    }
    Ok(ModifiedRequest {
        uri: uri.to_string(),
        query,
        headers,
    })
}

/// The `(name, value)` pairs one query string, header or cookie object carries: every value
/// of `multiValue` when the field has one, and its `value` otherwise. Header names are
/// lowercased. An absent object carries none.
fn values_of(object: Option<&Value>, member: &str) -> Result<Vec<(String, String)>, String> {
    let fields = match object {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(Value::Object(fields)) => fields,
        Some(_) => return Err(format!("the returned {member} is not an object")),
    };
    let lowercase = member == "headers";
    let mut pairs = Vec::new();
    for (name, field) in fields {
        let name = if lowercase { name.to_ascii_lowercase() } else { name.clone() };
        let text = |value: Option<&Value>| -> Result<String, String> {
            value
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| format!("the returned {member} field {name} carries no string value"))
        };
        match field.get("multiValue").and_then(Value::as_array) {
            Some(every) if !every.is_empty() => {
                for one in every {
                    pairs.push((name.clone(), text(one.get("value"))?));
                }
            }
            _ => pairs.push((name.clone(), text(field.get("value"))?)),
        }
    }
    Ok(pairs)
}

/// `text` with every byte a query component cannot carry percent-encoded, and an encoding
/// already present left as it is.
fn encoded(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        let kept = byte.is_ascii_alphanumeric() || b"-._~%!$'()*+,;:@/?".contains(&byte);
        if kept {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// The origin properties `cf.updateRequestOrigin` was last called with, as the edge applies
/// them; a member left out inherits the behaviour's origin.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct OriginUpdate {
    pub domain_name: Option<String>,
    pub host_header: Option<String>,
    pub origin_path: Option<String>,
    /// `customHeaders`: when present, it REPLACES the origin's own custom headers ("Empty
    /// object resets any header configured on the assigned origin").
    pub custom_headers: Option<Vec<(String, String)>>,
}

/// The members of `updateRequestOrigin` this emulator applies.
const APPLIED_ORIGIN_MEMBERS: [&str; 4] = ["domainName", "hostHeader", "originPath", "customHeaders"];

/// What the recorded `updateRequestOrigin` argument asks of the origin, or why it cannot be
/// applied.
///
/// `domainName` may carry a port, which the documented member does not allow: a load
/// balancer this emulator serves on a socket of its own publishes a `host:port` `DNSName`,
/// and that name is what a function reads out of its key value store.
pub fn origin_update(text: &str) -> Result<OriginUpdate, String> {
    let parsed: Value = serde_json::from_str(text)
        .map_err(|error| format!("the origin properties are not JSON: {error}"))?;
    let Value::Object(members) = parsed else {
        return Err("the origin properties are not an object".to_string());
    };
    if let Some(unapplied) = members
        .keys()
        .find(|member| !APPLIED_ORIGIN_MEMBERS.contains(&member.as_str()))
    {
        return Err(format!(
            "updateRequestOrigin named {unapplied}, which this emulator does not apply; it \
             applies {}",
            APPLIED_ORIGIN_MEMBERS.join(", ")
        ));
    }
    let string = |member: &str| -> Result<Option<String>, String> {
        match members.get(member) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(value)) => Ok(Some(value.clone())),
            Some(_) => Err(format!("updateRequestOrigin's {member} is not a string")),
        }
    };
    let domain_name = string("domainName")?;
    if let Some(domain) = &domain_name {
        let authority = domain
            .parse::<::http::uri::Authority>()
            .map_err(|_| format!("updateRequestOrigin's domainName {domain} is not a domain name"))?;
        if domain.is_empty() || domain.len() > MAX_ORIGIN_NAME_LEN || domain.contains('@') || authority.host().is_empty() {
            return Err(format!("updateRequestOrigin's domainName {domain} is not a domain name"));
        }
    }
    let host_header = string("hostHeader")?;
    if let Some(host) = &host_header {
        // "The host header can't include a colon (`:`) ... The host header can be up to 253
        // characters."
        let valid = !host.is_empty()
            && host.len() <= MAX_ORIGIN_NAME_LEN
            && !host.contains(':')
            && host.parse::<::http::HeaderValue>().is_ok()
            && !host.contains(char::is_whitespace);
        if !valid {
            return Err(format!("updateRequestOrigin's hostHeader {host} is not a host name"));
        }
    }
    let origin_path = string("originPath")?;
    if let Some(path) = &origin_path {
        // "The path should start with a forward slash (/) but shouldn't end with one."
        if !path.starts_with('/') || (path.len() > 1 && path.ends_with('/')) || path.len() > MAX_ORIGIN_PATH_LEN {
            return Err(format!("updateRequestOrigin's originPath {path} is not an origin path"));
        }
    }
    let custom_headers = match members.get("customHeaders") {
        None | Some(Value::Null) => None,
        Some(Value::Object(headers)) => Some(
            headers
                .iter()
                .map(|(name, value)| {
                    value
                        .as_str()
                        .map(|value| (name.to_ascii_lowercase(), value.to_string()))
                        .ok_or_else(|| format!("updateRequestOrigin's custom header {name} is not a string"))
                })
                .collect::<Result<Vec<_>, _>>()?,
        ),
        Some(_) => return Err("updateRequestOrigin's customHeaders is not an object".to_string()),
    };
    Ok(OriginUpdate {
        domain_name,
        host_header,
        origin_path,
        custom_headers,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> Headers {
        let mut headers = Headers::new();
        for (name, value) in pairs {
            headers.append(name.to_string(), value.to_string());
        }
        headers
    }

    /// The event object carries the request as CloudFront documents it: lowercased header
    /// names, the cookies apart from the headers, a repeated name under `multiValue` with
    /// its first value in `value`, and the query string one field per parameter.
    #[test]
    fn the_event_object_carries_the_viewer_request_as_cloudfront_documents_it() {
        let carried = headers(&[
            ("Host", "x.apps.example.com"),
            ("Accept", "application/json"),
            ("accept", "text/html"),
            ("Cookie", "session=abc; theme=dark"),
        ]);
        let request = ViewerRequest {
            distribution_domain_name: "d111111abcdef8.cloudfront.net",
            distribution_id: "EDFDVBD6EXAMPLE",
            request_id: "req-1",
            viewer_ip: "127.0.0.1",
            method: "GET",
            uri: "/media/index.mpd",
            query: "ID=42&NoValue=&mv=1&mv=2",
            headers: &carried,
        };
        let event: Value = serde_json::from_str(&event(&request)).expect("JSON");
        assert_eq!(
            event,
            json!({
                "version": "1.0",
                "context": {
                    "distributionDomainName": "d111111abcdef8.cloudfront.net",
                    "distributionId": "EDFDVBD6EXAMPLE",
                    "eventType": "viewer-request",
                    "requestId": "req-1",
                },
                "viewer": { "ip": "127.0.0.1" },
                "request": {
                    "method": "GET",
                    "uri": "/media/index.mpd",
                    "querystring": {
                        "ID": { "value": "42" },
                        "NoValue": { "value": "" },
                        "mv": { "value": "1", "multiValue": [{ "value": "1" }, { "value": "2" }] },
                    },
                    "headers": {
                        "host": { "value": "x.apps.example.com" },
                        "accept": {
                            "value": "application/json",
                            "multiValue": [{ "value": "application/json" }, { "value": "text/html" }],
                        },
                    },
                    "cookies": {
                        "session": { "value": "abc" },
                        "theme": { "value": "dark" },
                    },
                },
            })
        );
    }

    /// A returned request goes on with its uri, query string, headers and cookies; a
    /// returned response is served with its status, headers, cookies and body.
    #[test]
    fn a_returned_event_object_is_read_as_a_request_or_a_response() {
        let request = r#"{"request":{"method":"GET","uri":"/new","querystring":{"a":{"value":"1 2"},"b":{"value":"x","multiValue":[{"value":"x"},{"value":"y"}]}},"headers":{"Host":{"value":"x.example.com"}},"cookies":{"s":{"value":"1"}}}}"#;
        assert_eq!(
            answer(request),
            Ok(FunctionAnswer::Request(ModifiedRequest {
                uri: "/new".to_string(),
                query: "a=1%202&b=x&b=y".to_string(),
                headers: vec![
                    ("host".to_string(), "x.example.com".to_string()),
                    ("cookie".to_string(), "s=1".to_string()),
                ],
            }))
        );
        let string_query = r#"{"request":{"uri":"/","querystring":"z=9&y=8"}}"#;
        assert!(matches!(
            answer(string_query),
            Ok(FunctionAnswer::Request(ModifiedRequest { query, .. })) if query == "z=9&y=8"
        ));
        let response = r#"{"response":{"statusCode":404,"statusDescription":"Not Found","headers":{"content-type":{"value":"text/plain"}},"body":{"encoding":"base64","data":"bm9wZQ=="}}}"#;
        assert_eq!(
            answer(response),
            Ok(FunctionAnswer::Response(GeneratedResponse {
                status: 404,
                headers: vec![("content-type".to_string(), "text/plain".to_string())],
                body: b"nope".to_vec(),
            }))
        );
    }

    /// An answer CloudFront cannot use is refused rather than served.
    #[test]
    fn an_answer_cloudfront_cannot_use_is_refused() {
        let cases: [(&str, &str); 5] = [
            ("a uri without a leading slash", r#"{"request":{"uri":"relative"}}"#),
            ("no uri", r#"{"request":{"method":"GET"}}"#),
            ("a status that is no status", r#"{"response":{"statusCode":"200"}}"#),
            ("a header with no value", r#"{"request":{"uri":"/","headers":{"a":{}}}}"#),
            ("a body in an unknown encoding", r#"{"response":{"statusCode":200,"body":{"data":"x","encoding":"hex"}}}"#),
        ];
        for (case, output) in cases {
            assert!(answer(output).is_err(), "case {case:?}: expected a refusal, got {:?}", answer(output));
        }
    }

    /// The origin properties are applied when they are the ones this emulator applies, and
    /// refused otherwise.
    #[test]
    fn origin_properties_are_read_or_refused() {
        assert_eq!(
            origin_update(r#"{"domainName":"cell-0.elb.example.com","hostHeader":"x.example.com","originPath":"/app","customHeaders":{"X-Stage":"staging"}}"#),
            Ok(OriginUpdate {
                domain_name: Some("cell-0.elb.example.com".to_string()),
                host_header: Some("x.example.com".to_string()),
                origin_path: Some("/app".to_string()),
                custom_headers: Some(vec![("x-stage".to_string(), "staging".to_string())]),
            })
        );
        assert_eq!(
            origin_update(r#"{"domainName":"localhost:40123"}"#).map(|update| update.domain_name),
            Ok(Some("localhost:40123".to_string())),
            "a load balancer's host:port name is a domain this emulator routes"
        );
        let refused: [(&str, &str); 5] = [
            ("an unapplied member", r#"{"domainName":"a.example.com","originShield":{"enabled":false}}"#),
            ("a host header with a port", r#"{"hostHeader":"x.example.com:443"}"#),
            ("a domain with a path", r#"{"domainName":"a.example.com/path"}"#),
            ("an origin path ending in a slash", r#"{"originPath":"/app/"}"#),
            ("a domain that is not a string", r#"{"domainName":7}"#),
        ];
        for (case, text) in refused {
            assert!(origin_update(text).is_err(), "case {case:?}: expected a refusal, got {:?}", origin_update(text));
        }
    }
}
