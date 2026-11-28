// SPDX-License-Identifier: CC0-1.0

//! An Application Load Balancer passes a WebSocket on to its target, end to end through the
//! data port the binary serves.
//!
//! "Application Load Balancers provide native support for WebSockets. You can upgrade an
//! existing HTTP/1.1 connection into a WebSocket (ws or wss) connection by using an HTTP
//! connection upgrade" (Elastic Load Balancing User Guide, "Listeners for your Application
//! Load Balancers"). So the target receives the request still asking for the upgrade, its
//! `101 Switching Protocols` reaches the client, and after it the two exchange bytes on the
//! connection for as long as it lasts.
//!
//! The request is addressed as a reverse proxy in front of the emulator sends it: to the
//! emulator's endpoint, with the load balancer's DNS name as the `Host`.

mod support;

use std::time::Duration;

use support::{cell_load_balancer, on_authenticating_router, on_data_port};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// The handshake of RFC 6455, section 1.3: the key a client sends and the answer to it.
const WEBSOCKET_KEY: &str = "dGhlIHNhbXBsZSBub25jZQ==";
const WEBSOCKET_ACCEPT: &str = "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=";

/// A target that agrees to switch protocol when the request it reads asks for a WebSocket,
/// then sends back every byte it reads. A request that does not ask is answered 426, as a
/// WebSocket server answers one. Answers its port.
async fn websocket_target() -> u16 {
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
                        .write_all(b"HTTP/1.1 426 Upgrade Required\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                        .await;
                    return;
                }
                let agreed = format!(
                    "HTTP/1.1 101 Switching Protocols\r\nconnection: upgrade\r\nupgrade: websocket\r\nsec-websocket-accept: {WEBSOCKET_ACCEPT}\r\n\r\n"
                );
                if stream.write_all(agreed.as_bytes()).await.is_err() {
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

#[test]
fn a_websocket_opens_through_the_load_balancer_and_carries_bytes_both_ways() {
    on_authenticating_router(move |router, engine| {
        Box::pin(async move {
            let port = websocket_target().await;
            let (_, dns_name) = cell_load_balancer(&router, &engine, port).await;
            let (client, endpoint) = on_data_port(router).await;

            let answer = tokio::time::timeout(
                Duration::from_secs(10),
                client
                    .get(format!("https://{endpoint}/terminal"))
                    .header("host", dns_name)
                    .header("connection", "Upgrade")
                    .header("upgrade", "websocket")
                    .header("sec-websocket-version", "13")
                    .header("sec-websocket-key", WEBSOCKET_KEY)
                    .send(),
            )
            .await
            .expect("the handshake is answered within ten seconds")
            .expect("an answer");

            assert_eq!(answer.status(), 101, "the target agreed to switch protocol");
            let header = |name: &str| {
                answer
                    .headers()
                    .get(name)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_ascii_lowercase)
            };
            assert_eq!(header("upgrade").as_deref(), Some("websocket"));
            assert_eq!(header("connection").as_deref(), Some("upgrade"));
            assert_eq!(
                answer.headers().get("sec-websocket-accept").and_then(|value| value.to_str().ok()),
                Some(WEBSOCKET_ACCEPT),
                "the target's answer to the client's key reaches the client"
            );

            let mut connection = answer.upgrade().await.expect("the client's connection switches protocol");
            for sent in [&b"ping"[..], &b"a second message"[..]] {
                connection.write_all(sent).await.expect("the client writes on the switched connection");
                let mut echoed = vec![0u8; sent.len()];
                tokio::time::timeout(Duration::from_secs(10), connection.read_exact(&mut echoed))
                    .await
                    .expect("the target's bytes arrive within ten seconds")
                    .expect("the switched connection stays open");
                assert_eq!(echoed, sent, "the target sent back what the client sent");
            }
        })
    });
}
