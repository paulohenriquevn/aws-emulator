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

use std::time::{Duration, Instant};

use support::{cell_load_balancer, elb, event_stream_target, events_body, on_authenticating_router, on_data_port, EventStream as Target};

/// The idle timeout the load balancer is given, low so the cases finish quickly.
const IDLE_TIMEOUT_SECONDS: u64 = 2;

/// Where the client sends the request.
#[derive(Clone, Copy)]
enum Dialled {
    /// The emulator's endpoint, with the load balancer's DNS name as the `Host`.
    Endpoint,
    /// The load balancer's own address, its DNS name, over HTTP.
    OwnAddress,
}

/// Run `case` with a load balancer in front of a target behaving as `behaviour`, its idle
/// timeout set to [`IDLE_TIMEOUT_SECONDS`], and the router served on a data port. `case` is
/// handed a client and the request to send.
fn on_served_load_balancer(
    behaviour: Target,
    dialled: Dialled,
    case: impl FnOnce(reqwest::RequestBuilder) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()>>> + Send + 'static,
) {
    on_authenticating_router(move |router, engine| {
        Box::pin(async move {
            let port = event_stream_target(behaviour).await;
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

            let (client, endpoint) = on_data_port(router).await;

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
    on_served_load_balancer(Target::Events { events: 5, interval_seconds: 1 }, Dialled::Endpoint, |request| {
        Box::pin(five_events_one_by_one(request))
    });
}

#[test]
fn an_event_stream_reaches_the_client_event_by_event_on_the_load_balancers_own_address() {
    on_served_load_balancer(Target::Events { events: 5, interval_seconds: 1 }, Dialled::OwnAddress, |request| {
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
    assert_eq!(whole, events_body(5));
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
