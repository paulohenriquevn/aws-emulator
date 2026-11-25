// SPDX-License-Identifier: CC0-1.0

//! A CloudFront distribution relays its origin's answer as it arrives when it neither caches
//! nor compresses it, end to end through the data port the binary serves, with request
//! authentication and IAM enforcement on, as LoopZero's staging runs a tenant app: the
//! distribution's origin is an Application Load Balancer this emulator serves, forwarding to
//! an `ip` target that is a real HTTP server on a loopback port sending Server-Sent Events.
//!
//! The origin read timeout is "how long (in seconds) CloudFront waits for a response after
//! forwarding a request to the origin" and "how long (in seconds) CloudFront waits after
//! receiving a packet of a response from the origin and before receiving the next packet"
//! (CloudFront Developer Guide, "Origin settings", "Response timeout"). So a stream that
//! sends an event inside every read timeout lasts as long as it lasts, and one that goes
//! quiet for longer is cut.
//!
//! A behaviour that caches holds the answer whole before it serves it, so that case is
//! asserted as well: the cache needs the whole body.

mod support;

use std::time::{Duration, Instant};

use support::{
    cell_load_balancer, element, event_stream_target, events_body, on_authenticating_router, on_data_port, send,
    signed_by_emulator, EventStream, Router, CLOUDFRONT,
};

/// `Managed-CachingDisabled`, which caches nothing and enables no compression.
const CACHING_DISABLED: &str = "4135ea2d-6df8-44a3-9df3-4b5a84be39ad";

/// `Managed-CachingOptimized`, which caches a 200 for a day.
const CACHING_OPTIMIZED: &str = "658327ea-f89d-4fab-a63d-7e88639e58f6";

/// The origin read timeout the streaming cases give the origin, low so they finish quickly.
const READ_TIMEOUT_SECONDS: u64 = 3;

/// How far apart the streaming origin sends its events: inside the read timeout, and wide
/// enough that the first event's arrival is told apart from the second's sending.
const EVENT_INTERVAL_SECONDS: u64 = 2;

/// How many events the streaming origin sends. They span four seconds, longer than the read
/// timeout, so a timeout that counted the whole answer would cut them.
const EVENTS: u32 = 3;

/// A deployed distribution whose one origin is `origin` (a load balancer's DNS name, over
/// HTTP) with `read_timeout_seconds` as its `OriginReadTimeout`, and whose default behaviour
/// carries `cache_policy_id`; answers its id and its own domain name.
async fn distribution(
    router: &Router,
    reference: &str,
    origin: &str,
    cache_policy_id: &str,
    read_timeout_seconds: u64,
) -> (String, String) {
    let body = format!(
        "<DistributionConfig xmlns=\"http://cloudfront.amazonaws.com/doc/2020-05-31/\">\
         <CallerReference>{reference}</CallerReference>\
         <Origins><Quantity>1</Quantity><Items><Origin>\
         <Id>cell</Id><DomainName>{origin}</DomainName>\
         <CustomOriginConfig><HTTPPort>80</HTTPPort><HTTPSPort>443</HTTPSPort>\
         <OriginProtocolPolicy>http-only</OriginProtocolPolicy>\
         <OriginReadTimeout>{read_timeout_seconds}</OriginReadTimeout></CustomOriginConfig>\
         </Origin></Items></Origins>\
         <DefaultCacheBehavior><TargetOriginId>cell</TargetOriginId>\
         <ViewerProtocolPolicy>allow-all</ViewerProtocolPolicy>\
         <CachePolicyId>{cache_policy_id}</CachePolicyId>\
         </DefaultCacheBehavior>\
         <Comment>tenant app</Comment><PriceClass>PriceClass_All</PriceClass><Enabled>true</Enabled>\
         </DistributionConfig>"
    );
    let headers = [("content-type", "application/xml")];
    let created = send(
        router,
        signed_by_emulator(&CLOUDFRONT, "POST", "/2020-05-31/distribution", &headers, &body),
    )
    .await;
    assert_eq!(created.status, 201, "CreateDistribution: {}", created.text);
    let id = element(&created.text, "Id");
    let domain_name = element(&created.text, "DomainName");
    let path = format!("/2020-05-31/distribution/{id}");
    for _ in 0..100 {
        let read = send(router, signed_by_emulator(&CLOUDFRONT, "GET", &path, &[], "")).await;
        if element(&read.text, "Status") == "Deployed" {
            return (id, domain_name);
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    panic!("the distribution {id} did not deploy");
}

/// Where the viewer sends the request.
#[derive(Clone, Copy)]
enum Dialled {
    /// The emulator's endpoint, on the distribution's data-plane path, as a reverse proxy in
    /// front of the emulator forwards a viewer request.
    Endpoint,
    /// The distribution's own domain name, the address its own listener answers on, over
    /// HTTP.
    OwnAddress,
}

/// Run `case` with a distribution under `cache_policy_id` and `read_timeout_seconds` in front
/// of a load balancer whose target behaves as `behaviour`, and the router served on a data
/// port. `case` is handed the viewer request to send.
fn on_served_distribution(
    behaviour: EventStream,
    cache_policy_id: &'static str,
    read_timeout_seconds: u64,
    dialled: Dialled,
    case: impl FnOnce(reqwest::RequestBuilder) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()>>> + Send + 'static,
) {
    on_authenticating_router(move |router, engine| {
        Box::pin(async move {
            let port = event_stream_target(behaviour).await;
            let (_, cell) = cell_load_balancer(&router, &engine, port).await;
            let (id, domain_name) =
                distribution(&router, "tenant-app", &cell, cache_policy_id, read_timeout_seconds).await;

            let (client, endpoint) = on_data_port(router).await;

            let request = match dialled {
                Dialled::Endpoint => client
                    .get(format!("https://{endpoint}/_aws/cloudfront/{id}/events"))
                    .header("host", "x.apps.example.com"),
                Dialled::OwnAddress => client.get(format!("http://{domain_name}/events")),
            }
            .header("accept", "text/event-stream");
            case(request).await;
        })
    });
}

#[test]
fn an_event_stream_reaches_the_viewer_event_by_event() {
    // The events outlast the read timeout as a whole, but never leave the origin quiet for
    // one.
    on_served_distribution(
        EventStream::Events {
            events: EVENTS,
            interval_seconds: EVENT_INTERVAL_SECONDS,
        },
        CACHING_DISABLED,
        READ_TIMEOUT_SECONDS,
        Dialled::Endpoint,
        |request| Box::pin(events_one_by_one(request)),
    );
}

#[test]
fn an_event_stream_reaches_the_viewer_event_by_event_on_the_distributions_own_address() {
    on_served_distribution(
        EventStream::Events {
            events: EVENTS,
            interval_seconds: EVENT_INTERVAL_SECONDS,
        },
        CACHING_DISABLED,
        READ_TIMEOUT_SECONDS,
        Dialled::OwnAddress,
        |request| Box::pin(events_one_by_one(request)),
    );
}

/// Send `request` to a distribution whose origin sends [`EVENTS`] events
/// [`EVENT_INTERVAL_SECONDS`] apart, and check that the first arrives on its own before the
/// origin has sent the second, and that all of them arrive, in order.
async fn events_one_by_one(request: reqwest::RequestBuilder) {
    let sent = Instant::now();
    let mut answer = tokio::time::timeout(Duration::from_secs(10), request.send())
        .await
        .expect("the head arrives before the origin has finished")
        .expect("an answer");
    assert_eq!(answer.status(), 200);
    assert_eq!(
        answer
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("text/event-stream")
    );
    assert_eq!(
        answer.headers().get("x-cache").and_then(|value| value.to_str().ok()),
        Some("Miss from cloudfront")
    );
    let first = tokio::time::timeout(Duration::from_secs(10), answer.chunk())
        .await
        .expect("a first chunk")
        .expect("a readable body")
        .expect("a chunk before the end");
    let first_at = sent.elapsed();
    assert_eq!(
        String::from_utf8_lossy(&first),
        "data: 1\n\n",
        "the first chunk is the first event alone"
    );
    assert!(
        first_at < Duration::from_secs(EVENT_INTERVAL_SECONDS),
        "the first event arrived {first_at:?} after the request, so the edge held the answer \
         past the origin's second event"
    );
    let mut whole = String::from_utf8_lossy(&first).into_owned();
    while let Some(chunk) = tokio::time::timeout(Duration::from_secs(10), answer.chunk())
        .await
        .expect("the next chunk")
        .expect("a body that ends cleanly")
    {
        whole.push_str(&String::from_utf8_lossy(&chunk));
    }
    assert_eq!(whole, events_body(EVENTS));
}

#[test]
fn a_stream_that_goes_quiet_past_the_read_timeout_is_cut_after_the_events_it_sent() {
    // The head and the first event were relayed, so the edge can no longer answer 502: the
    // connection is closed, and the viewer sees a body that does not end cleanly. The load
    // balancer's own idle timeout is its default 60 seconds, so it is the edge that cuts.
    on_served_distribution(
        EventStream::StallsAfterOneEvent,
        CACHING_DISABLED,
        READ_TIMEOUT_SECONDS,
        Dialled::Endpoint,
        |request| {
            Box::pin(async move {
                let mut answer = tokio::time::timeout(Duration::from_secs(10), request.send())
                    .await
                    .expect("the head arrives while the origin stalls")
                    .expect("an answer");
                assert_eq!(answer.status(), 200);
                let first = tokio::time::timeout(Duration::from_secs(10), answer.chunk())
                    .await
                    .expect("a first chunk")
                    .expect("a readable body")
                    .expect("a chunk before the end");
                assert_eq!(String::from_utf8_lossy(&first), "data: 1\n\n");
                let quiet = Instant::now();
                let after = tokio::time::timeout(Duration::from_secs(10), answer.chunk())
                    .await
                    .expect("the stream is cut within the read timeout's reach");
                assert!(after.is_err(), "the stalled stream ended cleanly with {after:?}");
                let cut_at = quiet.elapsed();
                assert!(
                    cut_at < Duration::from_secs(READ_TIMEOUT_SECONDS + 3),
                    "the stream was cut {cut_at:?} after the first event, with a read timeout \
                     of {READ_TIMEOUT_SECONDS} s"
                );
            })
        },
    );
}

#[test]
fn a_behaviour_that_caches_serves_the_answer_whole() {
    // The cache holds the whole body, so the edge reads all of it before it answers: the first
    // byte reaches the viewer only once the origin has sent its last event, two seconds in.
    on_served_distribution(
        EventStream::Events {
            events: 3,
            interval_seconds: 1,
        },
        CACHING_OPTIMIZED,
        30,
        Dialled::Endpoint,
        |request| {
            Box::pin(async move {
                let sent = Instant::now();
                let answer = tokio::time::timeout(Duration::from_secs(20), request.send())
                    .await
                    .expect("an answer once the origin has finished")
                    .expect("an answer");
                let answered_at = sent.elapsed();
                assert_eq!(answer.status(), 200);
                assert_eq!(
                    answer.headers().get("x-cache").and_then(|value| value.to_str().ok()),
                    Some("Miss from cloudfront")
                );
                assert!(
                    answered_at >= Duration::from_millis(1500),
                    "the answer came {answered_at:?} after the request, before the origin had \
                 finished, so it was not held whole"
                );
                let whole = answer.text().await.expect("a whole body");
                assert_eq!(whole, events_body(3));
            })
        },
    );
}
