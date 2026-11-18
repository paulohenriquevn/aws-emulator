// SPDX-License-Identifier: CC0-1.0

//! fork: a `forward` to an `instance` or `ip` target group, PERFORMED.
//!
//! An Application Load Balancer opens a connection to the target it chose and passes the
//! client's request on to it. This module does that for a target group whose protocol is
//! HTTP: it dials the target's address on the port the registration gives (an `ip` target's
//! own address, or the private IPv4 address EC2 holds for an `instance` target), sends the
//! request over HTTP/1.1, and answers the client with what the target answered.
//!
//! What the load balancer changes on the way, as an Application Load Balancer does
//! ("HTTP headers and Application Load Balancers", Elastic Load Balancing User Guide): the
//! hop-by-hop headers are dropped, and `X-Forwarded-Proto` and `X-Forwarded-Port` name the
//! listener's protocol and port. `X-Forwarded-For` is passed on as the client sent it and is
//! not extended: a request served in process carries no client connection address to append.
//!
//! What it answers when the target does not: [`TARGET_UNREACHABLE_STATUS`] (502) when the
//! address cannot be resolved, the connection is refused, or the answer is not HTTP, and
//! [`TARGET_TIMEOUT_STATUS`] (504) when the target does not answer within the load
//! balancer's default idle timeout. An HTTPS target group is not dialled: this module speaks
//! no TLS to targets, so such a forward keeps the 502 it had.

use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper_util::rt::TokioIo;

use super::data_plane::{DataPlaneRequest, Served, TARGET_UNREACHABLE_STATUS};
use ::service_com_amazonaws_ec2_amazonec2::state as ec2;

/// What a load balancer answers when the target accepted the connection but did not answer
/// in time: "HTTP 504: Gateway Timeout".
pub const TARGET_TIMEOUT_STATUS: u16 = 504;

/// How long the connection to a target may take to open.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// How long the target may take to answer: the load balancer attribute
/// `idle_timeout.timeout_seconds` defaults to 60 seconds.
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(60);

/// The largest answer body this emulator relays, a bound of this module (an Application
/// Load Balancer states none). A larger answer is a 502.
pub const MAX_RESPONSE_BODY_BYTES: usize = 64 * 1024 * 1024;

/// The headers that describe one connection rather than the message, which a proxy does not
/// pass on (RFC 9110, section 7.6.1), plus the framing headers hyper writes itself.
const HOP_BY_HOP_HEADERS: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "content-length",
];

/// Which target a `forward` chose, and how to reach it.
#[derive(Debug, Clone)]
pub(super) struct Proxied {
    pub(super) target: TargetAddress,
    /// The port the registration receives traffic on.
    pub(super) port: u16,
    /// The target group's protocol; only HTTP is dialled.
    pub(super) target_protocol: Option<String>,
    /// The listener the request reached, which `X-Forwarded-Proto` and `X-Forwarded-Port`
    /// name.
    pub(super) listener_protocol: Option<String>,
    pub(super) listener_port: Option<i32>,
    pub(super) target_group_arn: String,
    /// The target-group stickiness cookie the answer hands back, for a client that does not
    /// hold it yet, as a `lambda` target's answer does.
    pub(super) set_cookie: Option<String>,
}

/// A registered target, as the target group's type names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum TargetAddress {
    /// An `ip` target: the registration IS the address.
    Address(String),
    /// An `instance` target: the instance id, whose address EC2 holds.
    Instance(String),
}

/// Why a target was not reached.
#[derive(Debug)]
enum Failure {
    Unreachable(String),
    TimedOut,
}

/// Pass `request` on to the target `proxied` names, and answer with what it answered.
pub(super) async fn served<Eng: Send + Sync + 'static>(
    engine: &Eng,
    proxied: &Proxied,
    request: &DataPlaneRequest,
    body: &[u8],
) -> Served {
    if !proxied
        .target_protocol
        .as_deref()
        .is_some_and(|protocol| protocol.eq_ignore_ascii_case("HTTP"))
    {
        tracing::warn!(
            target_group = %proxied.target_group_arn,
            protocol = ?proxied.target_protocol,
            "the target group's protocol is not HTTP, and this emulator dials targets over HTTP \
             only, so the target was not reached"
        );
        return Served::status(TARGET_UNREACHABLE_STATUS);
    }
    let Some(address) = target_address(engine, &proxied.target) else {
        tracing::warn!(
            target_group = %proxied.target_group_arn,
            target = ?proxied.target,
            "EC2 holds no running instance with a private address for the target, so the \
             connection to it cannot be made"
        );
        return Served::status(TARGET_UNREACHABLE_STATUS);
    };
    match exchange(&address, proxied, request, body).await {
        Ok(mut served) => {
            if let Some(field) = proxied.set_cookie.clone() {
                served.headers.push(("set-cookie".to_string(), field));
            }
            served
        }
        Err(Failure::TimedOut) => {
            tracing::warn!(target_group = %proxied.target_group_arn, %address, port = proxied.port, "the target did not answer in time");
            Served::status(TARGET_TIMEOUT_STATUS)
        }
        Err(Failure::Unreachable(reason)) => {
            tracing::warn!(target_group = %proxied.target_group_arn, %address, port = proxied.port, %reason, "the target was not reached");
            Served::status(TARGET_UNREACHABLE_STATUS)
        }
    }
}

/// The address to dial for `target`: an `ip` target's own, or the private IPv4 address of
/// a RUNNING instance EC2 holds. `None` when EC2 holds no such instance.
fn target_address<Eng: Send + Sync + 'static>(engine: &Eng, target: &TargetAddress) -> Option<String> {
    match target {
        TargetAddress::Address(address) => Some(address.clone()),
        TargetAddress::Instance(instance_id) => {
            let inventory = ec2::service_state(engine)?;
            let store = inventory.lock();
            store
                .instances
                .get(instance_id)
                .filter(|instance| instance.state == ec2::InstanceStateName::Running)
                .map(|instance| instance.private_ip.clone())
                .filter(|address| !address.is_empty())
        }
    }
}

async fn exchange(
    address: &str,
    proxied: &Proxied,
    request: &DataPlaneRequest,
    body: &[u8],
) -> Result<Served, Failure> {
    let stream = tokio::time::timeout(CONNECT_TIMEOUT, tokio::net::TcpStream::connect((address, proxied.port)))
        .await
        .map_err(|_| Failure::TimedOut)?
        .map_err(|error| Failure::Unreachable(format!("connect: {error}")))?;
    let outgoing = outgoing_request(proxied, request, body)?;
    let answer = tokio::time::timeout(RESPONSE_TIMEOUT, async move {
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .map_err(|error| Failure::Unreachable(format!("handshake: {error}")))?;
        let driver = tokio::spawn(async move {
            if let Err(error) = connection.await {
                tracing::debug!(%error, "the connection to the target ended with an error");
            }
        });
        let response = sender
            .send_request(outgoing)
            .await
            .map_err(|error| Failure::Unreachable(format!("request: {error}")))?;
        let (parts, incoming) = response.into_parts();
        let collected = Limited::new(incoming, MAX_RESPONSE_BODY_BYTES)
            .collect()
            .await
            .map_err(|error| Failure::Unreachable(format!("response body: {error}")))?
            .to_bytes();
        driver.abort();
        Ok::<_, Failure>((parts, collected))
    })
    .await
    .map_err(|_| Failure::TimedOut)??;
    let (parts, collected) = answer;
    Ok(Served {
        status: parts.status.as_u16(),
        headers: relayed_headers(&parts.headers),
        body: collected.to_vec(),
        ..Served::default()
    })
}

/// The request the target receives: the client's method, path, query, headers and body,
/// less the hop-by-hop headers, with the load balancer's forwarding headers.
fn outgoing_request(
    proxied: &Proxied,
    request: &DataPlaneRequest,
    body: &[u8],
) -> Result<hyper::Request<Full<Bytes>>, Failure> {
    let target = if request.query.is_empty() {
        request.path.clone()
    } else {
        format!("{}?{}", request.path, request.query)
    };
    let mut builder = hyper::Request::builder()
        .method(request.method.as_str())
        .uri(target);
    for (name, value) in forwarded_headers(proxied, request) {
        builder = builder.header(name, value);
    }
    builder
        .body(Full::new(Bytes::copy_from_slice(body)))
        .map_err(|error| Failure::Unreachable(format!("the request cannot be sent on: {error}")))
}

/// The headers the target receives, in the order the client sent them, then the load
/// balancer's own.
pub(super) fn forwarded_headers(proxied: &Proxied, request: &DataPlaneRequest) -> Vec<(String, String)> {
    let mut headers: Vec<(String, String)> = request
        .headers
        .iter()
        .filter(|(name, _)| !is_dropped_request_header(name))
        .cloned()
        .collect();
    // The host the client addressed, which the target serves virtual hosts by.
    headers.push(("host".to_string(), request.host.clone()));
    if let Some(protocol) = &proxied.listener_protocol {
        headers.push(("x-forwarded-proto".to_string(), protocol.to_ascii_lowercase()));
    }
    if let Some(port) = proxied.listener_port {
        headers.push(("x-forwarded-port".to_string(), port.to_string()));
    }
    headers
}

/// Whether a request header is one the target does not receive as the client sent it.
fn is_dropped_request_header(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    HOP_BY_HOP_HEADERS.contains(&name.as_str())
        || name == "host"
        || name == "x-forwarded-proto"
        || name == "x-forwarded-port"
        || name == super::data_plane::FORWARDED_HOST_HEADER
}

/// The target's answer headers the client receives: all of them but the hop-by-hop ones.
fn relayed_headers(headers: &hyper::HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .filter(|(name, _)| !HOP_BY_HOP_HEADERS.contains(&name.as_str()))
        .filter_map(|(name, value)| value.to_str().ok().map(|value| (name.as_str().to_string(), value.to_string())))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proxied() -> Proxied {
        Proxied {
            target: TargetAddress::Address("10.0.0.5".to_string()),
            port: 8080,
            target_protocol: Some("HTTP".to_string()),
            listener_protocol: Some("HTTP".to_string()),
            listener_port: Some(80),
            target_group_arn: "arn:aws:elasticloadbalancing:us-east-1:000000000000:targetgroup/t/1".to_string(),
            set_cookie: None,
        }
    }

    fn request(headers: &[(&str, &str)]) -> DataPlaneRequest {
        DataPlaneRequest {
            method: "GET".to_string(),
            path: "/a".to_string(),
            query: String::new(),
            host: "internal-echo-1.us-east-1.elb.amazonaws.com".to_string(),
            headers: headers.iter().map(|(n, v)| (n.to_string(), v.to_string())).collect(),
            port: None,
        }
    }

    #[test]
    fn the_target_gets_the_clients_headers_without_hop_by_hop_ones() {
        let headers = forwarded_headers(
            &proxied(),
            &request(&[("accept", "text/html"), ("connection", "keep-alive"), ("transfer-encoding", "chunked"), ("host", "127.0.0.1:9597")]),
        );
        assert!(headers.contains(&("accept".to_string(), "text/html".to_string())));
        assert!(!headers.iter().any(|(name, _)| name == "connection" || name == "transfer-encoding"));
        assert_eq!(headers.iter().filter(|(name, _)| name == "host").count(), 1);
    }

    #[test]
    fn the_target_is_told_the_listeners_protocol_and_port() {
        let headers = forwarded_headers(&proxied(), &request(&[("x-forwarded-proto", "https")]));
        assert!(headers.contains(&("host".to_string(), "internal-echo-1.us-east-1.elb.amazonaws.com".to_string())));
        assert!(headers.contains(&("x-forwarded-proto".to_string(), "http".to_string())));
        assert!(headers.contains(&("x-forwarded-port".to_string(), "80".to_string())));
        assert_eq!(headers.iter().filter(|(name, _)| name == "x-forwarded-proto").count(), 1);
    }

    #[test]
    fn a_client_x_forwarded_for_is_passed_on_unchanged() {
        let headers = forwarded_headers(&proxied(), &request(&[("x-forwarded-for", "203.0.113.7")]));
        assert!(headers.contains(&("x-forwarded-for".to_string(), "203.0.113.7".to_string())));
    }
}
