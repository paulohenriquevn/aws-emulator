// SPDX-License-Identifier: CC0-1.0

//! theo: the port a served DB instance publishes is the `Port` it was created with.
//!
//! RDS publishes `Endpoint.Port` as the port the instance listens on, which is the `Port`
//! the request set ("The port number on which the database accepts connections",
//! `CreateDBInstance`). The Terraform AWS provider reads its `port` attribute from
//! `Endpoint.Port`, so an instance that publishes an OS-assigned port instead shows a
//! change on every plan after the first apply. When the request names a `Port`, the listener
//! takes it on 127.0.0.1, or on another loopback address when that one is taken (so two
//! instances can both serve 5432), and falls back to the listener range only where no
//! extra loopback address routes. Without a `Port` the listener keeps a range port.

use std::net::TcpListener;

use tokio::net::TcpStream;

use crate::db_instance_engine_endpoint::available_instance;
use crate::db_instances::emulator;

/// A loopback port no listener holds right now.
fn free_port() -> u16 {
    TcpListener::bind(("127.0.0.1", 0))
        .and_then(|listener| listener.local_addr())
        .map(|addr| addr.port())
        .expect("the test host has a free loopback port")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_served_instance_listens_on_the_port_it_was_created_with() {
    for (case, engine) in [("postgres", "postgres"), ("mysql", "mysql")] {
        let router = emulator();
        let requested = free_port();
        let identifier = format!("emu-configured-port-{case}");
        let (address, published) = available_instance(
            &router,
            &identifier,
            engine,
            &[("Port", requested.to_string())],
        )
        .await;
        assert_eq!(
            published, requested,
            "case {case}: Endpoint.Port must be the Port the instance was created with"
        );
        TcpStream::connect((address.as_str(), published))
            .await
            .unwrap_or_else(|error| {
                panic!("case {case}: the published port must accept a connection: {error}")
            });
    }
}

/// Whether this host routes `127.0.0.2` to the loopback interface (Linux does; macOS
/// needs an alias). Where it does not, a taken port falls back to the listener range.
fn extra_loopback_available() -> bool {
    TcpListener::bind(("127.0.0.2", 0)).is_ok()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_port_another_process_holds_still_leaves_the_instance_reachable() {
    let router = emulator();
    let held = TcpListener::bind(("127.0.0.1", 0)).expect("a free loopback port");
    let held_port = held.local_addr().expect("a bound address").port();
    let (address, published) = available_instance(
        &router,
        "emu-configured-port-held",
        "postgres",
        &[("Port", held_port.to_string())],
    )
    .await;
    if extra_loopback_available() {
        assert_eq!(
            (published, address != "127.0.0.1"),
            (held_port, true),
            "case a held port: the instance keeps its Port on another loopback address, \
             got {address}:{published}"
        );
    } else {
        assert_ne!(
            published, held_port,
            "case a held port without extra loopback addresses: the instance falls back"
        );
    }
    TcpStream::connect((address.as_str(), published))
        .await
        .unwrap_or_else(|error| {
            panic!("case a held port: the published endpoint must accept a connection: {error}")
        });
    drop(held);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_instances_created_with_one_port_both_publish_it() {
    if !extra_loopback_available() {
        eprintln!("skipped: this host routes no extra loopback address");
        return;
    }
    let router = emulator();
    let port = free_port();
    let mut endpoints = Vec::new();
    for identifier in ["emu-shared-port-a", "emu-shared-port-b"] {
        let endpoint =
            available_instance(&router, identifier, "postgres", &[("Port", port.to_string())])
                .await;
        assert_eq!(
            endpoint.1, port,
            "case {identifier}: Endpoint.Port must be the Port it was created with"
        );
        TcpStream::connect((endpoint.0.as_str(), endpoint.1))
            .await
            .unwrap_or_else(|error| {
                panic!("case {identifier}: its endpoint must accept a connection: {error}")
            });
        endpoints.push(endpoint);
    }
    assert_ne!(
        endpoints[0].0, endpoints[1].0,
        "case two instances on one port: each needs its own address, got {endpoints:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_instance_created_without_a_port_keeps_a_listener_port_on_the_bind_address() {
    // Only a `Port` the request names is honoured. Without one the listener takes a port
    // from the core's range on the bind address, as upstream does, so parallel instances
    // that all default to 5432 never contend for it.
    let router = emulator();
    let (address, published) =
        available_instance(&router, "emu-default-port", "postgres", &[]).await;
    assert_eq!(
        address, "127.0.0.1",
        "case no Port: the endpoint stays on the bind address, got {address}:{published}"
    );
    TcpStream::connect((address.as_str(), published))
        .await
        .unwrap_or_else(|error| {
            panic!("case no Port: the published endpoint must accept a connection: {error}")
        });
}
