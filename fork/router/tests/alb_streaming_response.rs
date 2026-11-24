// SPDX-License-Identifier: CC0-1.0

//! An Application Load Balancer relays its target's answer as it arrives, and times the
//! target out on inactivity, end to end through the data port the binary serves.
//!
//! "The connection idle timeout is the period of time an existing client or target
//! connection can remain inactive, with no data being sent or received, before the load
//! balancer closes the connection" (Elastic Load Balancing User Guide, "Edit attributes for
//! your Application Load Balancer"). So a Server-Sent Events stream that sends a byte inside
//! every idle period stays open for as long as it lasts, and its events reach the client one
//! by one; a target that sends nothing for longer than the idle timeout is answered 504.
//!
//! The requests are addressed as a reverse proxy in front of the emulator sends them: to
//! the emulator's endpoint, with the load balancer's DNS name as the `Host`; one case dials
//! the load balancer's own address instead.

mod support;

use std::net::{Ipv4Addr, SocketAddr};
use std::time::{Duration, Instant};

use support::{cell_load_balancer, elb, on_authenticating_router};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// The idle timeout the load balancer is given, low so the cases finish quickly.
const IDLE_TIMEOUT_SECONDS: u64 = 2;

/// What the target does once it has read a request.
#[derive(Clone, Copy)]
enum Target {
    /// Sends the head and `events` events, one per second, then ends the body.
    Events { events: u32 },
    /// Sends the head and one event, then nothing, holding the connection open.
    StallsAfterOneEvent,
    /// Sends nothing, holding the connection open.
    Silent,
}

/// An HTTP target on a loopback port that behaves as `behaviour` says on `/events`, and
/// answers any other path, the health check's included, with an empty 200; answers its port.
async fn target(behaviour: Target) -> u16 {
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
                        Ok(0) | Err(_) => return,
                        Ok(read) => received.extend_from_slice(&buffer[..read]),
                    }
                }
                if !received.starts_with(b"GET /events ") {
                    let _ = stream.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n").await;
                    let _ = stream.shutdown().await;
                    return;
                }
                let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                            cache-control: no-cache\r\ntransfer-encoding: chunked\r\n\r\n";
                let event = |number: u32| {
                    let data = format!("data: {number}\n\n");
                    format!("{:x}\r\n{data}\r\n", data.len())
                };
                match behaviour {
                    Target::Events { events } => {
                        let _ = stream.write_all(head.as_bytes()).await;
                        for number in 1..=events {
                            let _ = stream.write_all(event(number).as_bytes()).await;
                            let _ = stream.flush().await;
                            if number < events {
                                tokio::time::sleep(Duration::from_secs(1)).await;
                            }
                        }
                        let _ = stream.write_all(b"0\r\n\r\n").await;
                        let _ = stream.shutdown().await;
                    }
                    Target::StallsAfterOneEvent => {
                        let _ = stream.write_all(head.as_bytes()).await;
                        let _ = stream.write_all(event(1).as_bytes()).await;
                        let _ = stream.flush().await;
                        tokio::time::sleep(Duration::from_secs(30)).await;
                    }
                    Target::Silent => tokio::time::sleep(Duration::from_secs(30)).await,
                }
            });
        }
    });
    port
}

/// Where the client sends the request.
#[derive(Clone, Copy)]
enum Dialled {
    /// The emulator's endpoint, with the load balancer's DNS name as the `Host`.
    Endpoint,
    /// The load balancer's own address, its DNS name, over HTTP.
    OwnAddress,
}

/// A loopback port nothing listens on.
fn free_port() -> u16 {
    let probe = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("a loopback port");
    probe.local_addr().expect("a bound address").port()
}

/// Run `case` with a load balancer in front of a target behaving as `behaviour`, its idle
/// timeout set to [`IDLE_TIMEOUT_SECONDS`], and the router served on a data port. `case` is
/// handed a client and the request to send.
fn on_served_load_balancer(
    behaviour: Target,
    dialled: Dialled,
    case: impl FnOnce(reqwest::RequestBuilder) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()>>> + Send + 'static,
) {
    // The served data port writes its client files under the home directory; this test's
    // go to the target directory rather than the developer's.
    std::env::set_var("HOME", env!("CARGO_TARGET_TMPDIR"));
    on_authenticating_router(move |router, engine| {
        Box::pin(async move {
            let port = target(behaviour).await;
            let (arn, dns_name) = cell_load_balancer(&router, &engine, port).await;
            let idle = IDLE_TIMEOUT_SECONDS.to_string();
            elb(
                &router,
                &[
                    ("Action", "ModifyLoadBalancerAttributes"),
                    ("LoadBalancerArn", &arn),
                    ("Attributes.member.1.Key", "idle_timeout.timeout_seconds"),
                    ("Attributes.member.1.Value", &idle),
                ],
            )
            .await;

            let data_port = free_port();
            tokio::spawn(emulator_server::serve_emulator_on(router, Ipv4Addr::LOCALHOST.into(), data_port));
            let root = emulator_server::TlsCertificateAuthority::for_this_process().der().to_vec();
            let client = reqwest::Client::builder()
                .tls_built_in_root_certs(false)
                .add_root_certificate(reqwest::Certificate::from_der(&root).expect("the emulator's certificate"))
                .build()
                .expect("a TLS client");
            let endpoint = SocketAddr::from((Ipv4Addr::LOCALHOST, data_port));
            let health = format!("https://{endpoint}/__admin/health");
            let mut last = String::from("no attempt");
            for _ in 0..100 {
                match client.get(&health).send().await {
                    Ok(answer) if answer.status().is_success() => {
                        last.clear();
                        break;
                    }
                    Ok(answer) => last = format!("status {}", answer.status()),
                    Err(error) => last = format!("{error:?}"),
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            assert!(last.is_empty(), "the data port did not answer: {last}");

            let request = match dialled {
                Dialled::Endpoint => client.get(format!("https://{endpoint}/events")).header("host", dns_name),
                Dialled::OwnAddress => client.get(format!("http://{dns_name}/events")),
            }
            .header("accept", "text/event-stream");
            case(request).await;
        })
    });
}

#[test]
fn an_event_stream_reaches_the_client_event_by_event() {
    // Five events a second apart outlast the two-second idle timeout as a whole, but never
    // leave the connection idle for one.
    on_served_load_balancer(Target::Events { events: 5 }, Dialled::Endpoint, |request| {
        Box::pin(five_events_one_by_one(request))
    });
}

#[test]
fn an_event_stream_reaches_the_client_event_by_event_on_the_load_balancers_own_address() {
    on_served_load_balancer(Target::Events { events: 5 }, Dialled::OwnAddress, |request| {
        Box::pin(five_events_one_by_one(request))
    });
}

/// Send `request` to a target that sends five events a second apart, and check that the
/// first arrives before the target has finished and that all five arrive, in order.
async fn five_events_one_by_one(request: reqwest::RequestBuilder) {
    let sent = Instant::now();
    let mut answer = tokio::time::timeout(Duration::from_secs(10), request.send())
        .await
        .expect("the head arrives before the target has finished")
        .expect("an answer");
    assert_eq!(answer.status(), 200);
    assert_eq!(
        answer.headers().get("content-type").and_then(|value| value.to_str().ok()),
        Some("text/event-stream")
    );
    let first = tokio::time::timeout(Duration::from_secs(10), answer.chunk())
        .await
        .expect("a first chunk")
        .expect("a readable body")
        .expect("a chunk before the end");
    let first_at = sent.elapsed();
    assert!(
        String::from_utf8_lossy(&first).starts_with("data: 1\n\n"),
        "the first chunk was {first:?}"
    );
    assert!(
        first_at < Duration::from_secs(2),
        "the first event arrived {first_at:?} after the request, so the answer was held \
         until the target finished"
    );
    let mut whole = String::from_utf8_lossy(&first).into_owned();
    while let Some(chunk) = tokio::time::timeout(Duration::from_secs(10), answer.chunk())
        .await
        .expect("the next chunk")
        .expect("a body that ends cleanly")
    {
        whole.push_str(&String::from_utf8_lossy(&chunk));
    }
    let expected: String = (1..=5).map(|number| format!("data: {number}\n\n")).collect();
    assert_eq!(whole, expected);
}

#[test]
fn a_target_silent_past_the_idle_timeout_is_answered_504() {
    on_served_load_balancer(Target::Silent, Dialled::Endpoint, |request| {
        Box::pin(async move {
            let sent = Instant::now();
            let answer = tokio::time::timeout(Duration::from_secs(10), request.send())
                .await
                .expect("an answer within the idle timeout's reach, not the 60-second default")
                .expect("an answer");
            let answered_at = sent.elapsed();
            assert_eq!(answer.status(), 504);
            assert!(
                answered_at >= Duration::from_secs(IDLE_TIMEOUT_SECONDS) && answered_at < Duration::from_secs(5),
                "answered {answered_at:?} after the request, with an idle timeout of \
                 {IDLE_TIMEOUT_SECONDS} s"
            );
        })
    });
}

#[test]
fn a_stream_that_goes_idle_is_cut_after_the_events_it_sent() {
    // The head and the first event were relayed, so the 504 can no longer be answered: the
    // connection is closed, and the client sees a body that does not end cleanly.
    on_served_load_balancer(Target::StallsAfterOneEvent, Dialled::Endpoint, |request| {
        Box::pin(async move {
            let mut answer = tokio::time::timeout(Duration::from_secs(10), request.send())
                .await
                .expect("the head arrives while the target stalls")
                .expect("an answer");
            assert_eq!(answer.status(), 200);
            let first = tokio::time::timeout(Duration::from_secs(10), answer.chunk())
                .await
                .expect("a first chunk")
                .expect("a readable body")
                .expect("a chunk before the end");
            assert_eq!(String::from_utf8_lossy(&first), "data: 1\n\n");
            let after = tokio::time::timeout(Duration::from_secs(10), answer.chunk())
                .await
                .expect("the stream is cut within the idle timeout's reach");
            assert!(after.is_err(), "the stalled stream ended cleanly with {after:?}");
        })
    });
}
