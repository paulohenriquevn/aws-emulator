// SPDX-License-Identifier: CC0-1.0

//! fork: a `forward` to an `ip` or `instance` target group passes the request on to the
//! target and answers with what the target answered.
//!
//! Each case runs a real HTTP server on a loopback port, registers it as the `ip` target of
//! an HTTP target group, and sends traffic to the load balancer by the name it published.
//! The server answers with the request it received, so every assertion about what the
//! target got is made on what the target itself saw.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use ::service_com_amazonaws_ec2_amazonec2::ops::types as ec2_types;
use ::service_com_amazonaws_ec2_amazonec2::state as ec2;
use service_com_amazonaws_elasticloadbalancingv2_elasticloadbalancing_v10::state::ComAmazonawsElasticloadbalancingv2ElasticloadbalancingV10State as ElbState;

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
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(echo(stream));
        }
    });
    port
}

/// The same server over TLS, with a self-signed certificate for `localhost`: one no client
/// that validates certificates would accept for `127.0.0.1`.
async fn tls_echo_server() -> u16 {
    let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).expect("a self-signed certificate");
    let key = rustls::pki_types::PrivateKeyDer::Pkcs8(certified.key_pair.serialize_der().into());
    let config = rustls::ServerConfig::builder_with_provider(std::sync::Arc::new(rustls::crypto::aws_lc_rs::default_provider()))
        .with_safe_default_protocol_versions()
        .expect("the default TLS versions")
        .with_no_client_auth()
        .with_single_cert(vec![certified.cert.der().clone()], key)
        .expect("the certificate and its key");
    let acceptor = tokio_rustls::TlsAcceptor::from(std::sync::Arc::new(config));
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("a loopback port");
    let port = listener.local_addr().expect("a bound address").port();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                if let Ok(tls) = acceptor.accept(stream).await {
                    echo(tls).await;
                }
            });
        }
    });
    port
}

/// Answer one request on `stream` with the request itself.
async fn echo<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(mut stream: S) {
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
}

/// A plain HTTP server meeting a TLS client, as nginx does: it reads what arrives, answers
/// a plaintext 400 and closes, which fails the client's handshake.
async fn plaintext_server() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("a loopback port");
    let port = listener.local_addr().expect("a bound address").port();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut hello = [0u8; 512];
                let _ = stream.read(&mut hello).await;
                let _ = stream
                    .write_all(b"HTTP/1.1 400 Bad Request\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                    .await;
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
    forwarding_over(target_port, "HTTP").await
}

/// [`forwarding_to`], with a target group of `protocol`.
async fn forwarding_over(target_port: u16, protocol: &str) -> (MultiServiceRouter, String) {
    let (router, _, _, dns_name) = forwarding_setup(target_port, protocol).await;
    (router, dns_name)
}

/// [`forwarding_over`], answering the engine and the load balancer's ARN as well.
async fn forwarding_setup(
    target_port: u16,
    protocol: &str,
) -> (MultiServiceRouter, Arc<engine::InMemoryEngine>, String, String) {
    let (router, engine) = emulator();
    let (lb_arn, dns_name) = create_load_balancer(&router, "forwarding").await;

    let mut group = action("CreateTargetGroup");
    group.push(p("Name", "forwarding-ip"));
    group.push(p("Protocol", protocol));
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
    (router, engine, lb_arn, dns_name)
}

/// The security group of the load balancer, and the one guarding the target.
const LOAD_BALANCER_GROUP: &str = "sg-0a1b2c3d4e5f60001";
const TARGET_GROUP_SG: &str = "sg-0a1b2c3d4e5f60002";

/// Put the load balancer in [`LOAD_BALANCER_GROUP`], and the target's address on an
/// interface in [`TARGET_GROUP_SG`], whose one inbound rule admits TCP `from..=to` from the
/// load balancer's group.
fn guard_target(engine: &Arc<engine::InMemoryEngine>, lb_arn: &str, from: i32, to: i32) {
    {
        let state = engine.state::<ElbState>();
        let mut store = state.lock();
        store
            .load_balancers
            .get_mut(lb_arn)
            .expect("the load balancer")
            .security_groups = vec![LOAD_BALANCER_GROUP.to_string()];
        // The health check runs on the port the group admits, so a periodic check keeps the
        // target healthy and the request reaches the traffic-port decision.
        for group in store.target_groups.values_mut() {
            group.health_check.port = Some(from.to_string());
        }
    }
    let state = ec2::service_state(&**engine).expect("the shared engine holds an EC2 inventory");
    let mut store = state.lock();
    for (id, name, rules) in [
        (LOAD_BALANCER_GROUP, "load-balancer", Vec::new()),
        (
            TARGET_GROUP_SG,
            "target",
            vec![ec2_types::IpPermission {
                ip_protocol: Some("tcp".to_string()),
                from_port: Some(from),
                to_port: Some(to),
                user_id_group_pairs: Some(vec![ec2_types::UserIdGroupPair {
                    group_id: Some(LOAD_BALANCER_GROUP.to_string()),
                    ..ec2_types::UserIdGroupPair::default()
                }]),
                ..ec2_types::IpPermission::default()
            }],
        ),
    ] {
        store.security_groups.insert(
            id.to_string(),
            ec2_types::SecurityGroup {
                group_id: Some(id.to_string()),
                group_name: Some(name.to_string()),
                vpc_id: Some("vpc-12345678".to_string()),
                ip_permissions: Some(rules),
                ..ec2_types::SecurityGroup::default()
            },
        );
    }
    store
        .insert_network_interface(ec2::NetworkInterfaceSpec {
            subnet_id: "subnet-11111111".to_string(),
            vpc_id: "vpc-12345678".to_string(),
            availability_zone: "us-east-1a".to_string(),
            region: "us-east-1".to_string(),
            description: "the target".to_string(),
            interface_type: ec2::NETWORK_INTERFACE_TYPE_INTERFACE.to_string(),
            private_ip_addresses: vec!["127.0.0.1".to_string()],
            groups: vec![ec2_types::GroupIdentifier {
                group_id: Some(TARGET_GROUP_SG.to_string()),
                group_name: Some("target".to_string()),
            }],
            ..ec2::NetworkInterfaceSpec::default()
        })
        .expect("the inventory holds the interface");
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

#[tokio::test]
async fn an_https_target_with_a_self_signed_certificate_answers_through_the_load_balancer() {
    let (router, dns_name) = forwarding_over(tls_echo_server().await, "HTTPS").await;

    let (status, headers, body) = through(&router, &dns_name, "GET", "/secure", &[], b"").await;

    assert_eq!(status, 200, "the target's own status over TLS, got body {body:?}");
    assert!(headers.contains(&("x-backend".to_string(), "echo".to_string())), "{headers:?}");
    assert!(body.to_ascii_lowercase().starts_with("get /secure http/1.1\r\n"), "{body:?}");
}

#[tokio::test]
async fn an_https_target_that_speaks_no_tls_is_a_bad_gateway() {
    let (router, dns_name) = forwarding_over(plaintext_server().await, "HTTPS").await;

    let (status, _, _) = through(&router, &dns_name, "GET", "/", &[], b"").await;

    assert_eq!(status, 502);
}

#[tokio::test]
async fn a_target_whose_security_group_admits_only_another_port_is_a_bad_gateway() {
    let target_port = echo_server().await;
    let (router, engine, lb_arn, dns_name) = forwarding_setup(target_port, "HTTP").await;
    // Admits the load balancer on a port that is not the traffic port, as a group opened
    // only for a separate health-check port would.
    let other = if target_port == 1 { 2 } else { 1 };
    guard_target(&engine, &lb_arn, other, other);

    let (status, headers, _) = through(&router, &dns_name, "GET", "/", &[], b"").await;

    assert_eq!(status, 502);
    assert!(!headers.iter().any(|(name, _)| name == "x-backend"), "the target was not reached: {headers:?}");
}

#[tokio::test]
async fn a_target_whose_security_group_admits_the_traffic_port_answers() {
    let target_port = echo_server().await;
    let (router, engine, lb_arn, dns_name) = forwarding_setup(target_port, "HTTP").await;
    guard_target(&engine, &lb_arn, i32::from(target_port), i32::from(target_port));

    let (status, _, body) = through(&router, &dns_name, "GET", "/", &[], b"").await;

    assert_eq!(status, 200, "{body:?}");
}

/// A server that agrees to switch protocol when asked the way a WebSocket client asks, then
/// sends back every byte it reads on the switched connection. Asked any other way it
/// answers 400. Answers its port.
async fn upgrading_server() -> u16 {
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
                while !received.windows(4).any(|window| window == b"\r\n\r\n") {
                    match stream.read(&mut buffer).await {
                        Ok(read) if read > 0 => received.extend_from_slice(&buffer[..read]),
                        _ => return,
                    }
                }
                let head = String::from_utf8_lossy(&received).to_ascii_lowercase();
                if !(head.contains("\r\nconnection: upgrade\r\n") && head.contains("\r\nupgrade: websocket\r\n")) {
                    let _ = stream
                        .write_all(b"HTTP/1.1 400 Bad Request\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                        .await;
                    return;
                }
                let agreed = b"HTTP/1.1 101 Switching Protocols\r\nconnection: upgrade\r\nupgrade: websocket\r\nsec-websocket-accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n\r\n";
                if stream.write_all(agreed).await.is_err() {
                    return;
                }
                loop {
                    match stream.read(&mut buffer).await {
                        Ok(read) if read > 0 => {
                            if stream.write_all(&buffer[..read]).await.is_err() {
                                return;
                            }
                        }
                        _ => return,
                    }
                }
            });
        }
    });
    port
}

/// A request that asks for a WebSocket, as a listener hands it to the load balancer: with
/// the client's connection left in a switch. Answers the client's end of that connection.
fn websocket_request(dns_name: &str) -> (http::Request<bytes::Bytes>, emulator_server::ProtocolSwitch, tokio::io::DuplexStream) {
    let (client, listener_side) = tokio::io::duplex(1024);
    let switch = emulator_server::ProtocolSwitch::new(async move { Ok(listener_side) });
    let mut request = http::Request::builder()
        .method("GET")
        .uri(format!("http://{dns_name}/terminal"))
        .header("host", dns_name)
        .header("connection", "Upgrade")
        .header("upgrade", "websocket")
        .header("sec-websocket-version", "13")
        .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
        .body(bytes::Bytes::new())
        .expect("a well-formed request");
    request.extensions_mut().insert(switch.clone());
    (request, switch, client)
}

/// A WebSocket did not get through a load balancer: the target received the request
/// without its `Connection` header and refused it, so a terminal behind one never opened.
#[tokio::test]
async fn a_websocket_upgrade_reaches_the_target_and_the_two_connections_are_joined() {
    let (router, dns_name) = forwarding_to(upgrading_server().await).await;
    let (request, switch, mut client) = websocket_request(&dns_name);

    let response = router.dispatch(request).await;

    assert_eq!(response.status().as_u16(), 101, "the target agreed to switch, so the load balancer answers 101");
    assert_eq!(switch.accepted().as_deref(), Some("websocket"), "the listener is told the protocol to answer with");
    assert_eq!(
        response.headers().get("sec-websocket-accept").and_then(|value| value.to_str().ok()),
        Some("s3pPLMBiTxaQ9kYGzzhZRbK+xOo="),
        "the target's handshake answer reaches the client"
    );
    client.write_all(b"ping").await.expect("the client writes on its switched connection");
    let mut echoed = [0u8; 4];
    tokio::time::timeout(Duration::from_secs(5), client.read_exact(&mut echoed))
        .await
        .expect("the target's bytes arrive within five seconds")
        .expect("the switched connection stays open");
    assert_eq!(&echoed, b"ping", "what the client sends reaches the target, and what the target sends reaches the client");
}

#[tokio::test]
async fn a_target_that_does_not_switch_protocol_leaves_the_clients_connection_alone() {
    let (router, dns_name) = forwarding_to(echo_server().await).await;
    let (request, switch, _client) = websocket_request(&dns_name);

    let response = router.dispatch(request).await;

    assert_eq!(response.status().as_u16(), 200, "the target answered the request as plain HTTP");
    assert_eq!(switch.accepted(), None, "no protocol was switched, so the listener writes an ordinary answer");
}
