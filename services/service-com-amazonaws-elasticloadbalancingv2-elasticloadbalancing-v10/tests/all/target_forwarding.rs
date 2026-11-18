// SPDX-License-Identifier: CC0-1.0

//! fork: a `forward` to an `ip` or `instance` target group passes the request on to the
//! target and answers with what the target answered.
//!
//! Each case runs a real HTTP server on a loopback port, registers it as the `ip` target of
//! an HTTP target group, and sends traffic to the load balancer by the name it published.
//! The server answers with the request it received, so every assertion about what the
//! target got is made on what the target itself saw.

use std::time::{Duration, SystemTime};

use smithy_server_runtime::MultiServiceRouter;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::alb_data_plane::{create_load_balancer, settle};
use crate::provisioning_loop::{action, call, emulator, p};

/// Run an HTTP/1.1 server on a loopback port that answers every request with the request
/// it read (head and body) as its body, and `x-backend: echo`. Answers its port.
async fn echo_server() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("a loopback port");
    let port = listener.local_addr().expect("a bound address").port();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut received = Vec::new();
                let mut buffer = [0u8; 4096];
                let head_end = loop {
                    let Ok(read) = stream.read(&mut buffer).await else {
                        return;
                    };
                    if read == 0 {
                        return;
                    }
                    received.extend_from_slice(&buffer[..read]);
                    if let Some(end) = received.windows(4).position(|window| window == b"\r\n\r\n") {
                        break end + 4;
                    }
                };
                let head = String::from_utf8_lossy(&received[..head_end]).to_ascii_lowercase();
                let length: usize = head
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length:"))
                    .and_then(|value| value.trim().parse().ok())
                    .unwrap_or(0);
                while received.len() < head_end + length {
                    let Ok(read) = stream.read(&mut buffer).await else {
                        return;
                    };
                    if read == 0 {
                        break;
                    }
                    received.extend_from_slice(&buffer[..read]);
                }
                let answer = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\nx-backend: echo\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    received.len()
                );
                let _ = stream.write_all(answer.as_bytes()).await;
                let _ = stream.write_all(&received).await;
                let _ = stream.shutdown().await;
            });
        }
    });
    port
}

/// A port nothing listens on: bound once, then released.
async fn closed_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("a loopback port");
    listener.local_addr().expect("a bound address").port()
}

/// An active load balancer whose HTTP:80 listener forwards everything to an `ip` target
/// group holding `127.0.0.1:<target_port>`. Answers the router and the published DNS name.
async fn forwarding_to(target_port: u16) -> (MultiServiceRouter, String) {
    let (router, engine) = emulator();
    let (lb_arn, dns_name) = create_load_balancer(&router, "forwarding").await;

    let mut group = action("CreateTargetGroup");
    group.push(p("Name", "forwarding-ip"));
    group.push(p("Protocol", "HTTP"));
    group.push(p("Port", &target_port.to_string()));
    group.push(p("VpcId", "vpc-12345678"));
    group.push(p("TargetType", "ip"));
    let created = call(&router, &group).await;
    assert_eq!(created.status, 200, "CreateTargetGroup: {}", created.body);
    let group_arn = created.text("TargetGroupArn").expect("a target group ARN");

    let mut register = action("RegisterTargets");
    register.push(p("TargetGroupArn", &group_arn));
    register.push(p("Targets.member.1.Id", "127.0.0.1"));
    let registered = call(&router, &register).await;
    assert_eq!(registered.status, 200, "RegisterTargets: {}", registered.body);

    let mut listener = action("CreateListener");
    listener.push(p("LoadBalancerArn", &lb_arn));
    listener.push(p("Protocol", "HTTP"));
    listener.push(p("Port", "80"));
    listener.push(p("DefaultActions.member.1.Type", "forward"));
    listener.push(p("DefaultActions.member.1.TargetGroupArn", &group_arn));
    let created = call(&router, &listener).await;
    assert_eq!(created.status, 200, "CreateListener: {}", created.body);

    settle(&engine, &lb_arn, SystemTime::now() - Duration::from_secs(1));
    (router, dns_name)
}

/// One request to the load balancer by its published name.
async fn through(
    router: &MultiServiceRouter,
    dns_name: &str,
    method: &str,
    target: &str,
    headers: &[(&str, &str)],
    body: &'static [u8],
) -> (u16, Vec<(String, String)>, String) {
    let mut builder = http::Request::builder()
        .method(method)
        .uri(format!("http://{dns_name}{target}"))
        .header("host", dns_name);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let request = builder
        .body(bytes::Bytes::from_static(body))
        .expect("a well-formed request");
    let response = router.dispatch(request).await;
    let status = response.status().as_u16();
    let headers = response
        .headers()
        .iter()
        .filter_map(|(name, value)| value.to_str().ok().map(|value| (name.as_str().to_string(), value.to_string())))
        .collect();
    (status, headers, String::from_utf8_lossy(response.body()).into_owned())
}

#[tokio::test]
async fn a_forwarded_request_is_answered_by_the_target() {
    let (router, dns_name) = forwarding_to(echo_server().await).await;

    let (status, headers, body) =
        through(&router, &dns_name, "GET", "/hello?greeting=hi", &[("accept", "text/plain"), ("connection", "keep-alive")], b"").await;

    assert_eq!(status, 200, "the target's own status, got body {body:?}");
    assert!(headers.contains(&("x-backend".to_string(), "echo".to_string())), "the target's headers reach the client: {headers:?}");
    let seen = body.to_ascii_lowercase();
    assert!(seen.starts_with("get /hello?greeting=hi http/1.1\r\n"), "the target got the client's method, path and query: {body:?}");
    assert!(seen.contains("\r\naccept: text/plain\r\n"), "the target got the client's headers: {body:?}");
    assert!(seen.contains("\r\nx-forwarded-proto: http\r\n"), "the target is told the listener's protocol: {body:?}");
    assert!(seen.contains("\r\nx-forwarded-port: 80\r\n"), "the target is told the listener's port: {body:?}");
    assert!(!seen.contains("keep-alive"), "a hop-by-hop header is not passed on: {body:?}");
}

#[tokio::test]
async fn a_request_body_reaches_the_target() {
    let (router, dns_name) = forwarding_to(echo_server().await).await;

    let (status, _, body) =
        through(&router, &dns_name, "POST", "/orders", &[("content-type", "application/json")], br#"{"item":"book"}"#).await;

    assert_eq!(status, 200, "got body {body:?}");
    assert!(body.to_ascii_lowercase().starts_with("post /orders http/1.1\r\n"), "{body:?}");
    assert!(body.ends_with(r#"{"item":"book"}"#), "the target got the client's body: {body:?}");
}

#[tokio::test]
async fn a_target_that_refuses_the_connection_is_a_bad_gateway() {
    let (router, dns_name) = forwarding_to(closed_port().await).await;

    let (status, _, _) = through(&router, &dns_name, "GET", "/", &[], b"").await;

    assert_eq!(status, 502);
}
