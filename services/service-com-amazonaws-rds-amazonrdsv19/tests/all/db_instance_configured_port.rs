// SPDX-License-Identifier: CC0-1.0

//! theo: the port a served DB instance publishes is the `Port` it was created with.
//!
//! RDS publishes `Endpoint.Port` as the port the instance listens on, which is the `Port`
//! the request set ("The port number on which the database accepts connections",
//! `CreateDBInstance`). The Terraform AWS provider reads its `port` attribute from
//! `Endpoint.Port`, so an instance that publishes an OS-assigned port instead shows a
//! change on every plan after the first apply. The listener now takes the configured port
//! when it is free and falls back to the configured listener range when another process
//! holds it, so the instance stays reachable either way.

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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_port_another_process_holds_falls_back_to_a_reachable_one() {
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
    assert_ne!(
        published, held_port,
        "case a held port: the instance cannot listen where another process already does"
    );
    TcpStream::connect((address.as_str(), published))
        .await
        .unwrap_or_else(|error| {
            panic!("case a held port: the fallback port must accept a connection: {error}")
        });
    drop(held);
}
