// SPDX-License-Identifier: CC0-1.0

//! fork: a client connection a plane takes over once its target agrees to change protocol.
//!
//! The [`crate::EmulatorService`] contract answers one request with one response. A
//! WebSocket is different: the client asks to upgrade the connection, the server answers
//! `101 Switching Protocols`, and from then on the two exchange frames on the same
//! connection for as long as it lasts. A load balancer passes that request on to its target
//! and, when the target agrees, joins the client's connection to the target's.
//!
//! So a request that arrives on a network listener and asks for an upgrade carries a
//! [`ProtocolSwitch`] among its extensions. A plane whose target answered `101` calls
//! [`ProtocolSwitch::accept`] with the protocol the target named, which hands it the
//! client's connection, and answers `101` itself. The listener then writes that answer with
//! the `Connection` and `Upgrade` headers of the switch, and the connection the plane was
//! handed becomes readable once the answer is on the wire.
//!
//! A request made in process carries no switch: there is no client connection to hand over,
//! and a plane answers it as it would any other request.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};

use tokio::io::{AsyncRead, AsyncWrite};

/// A connection after its protocol was switched: bytes in both directions.
pub trait SwitchedStream: AsyncRead + AsyncWrite + Send + Unpin {}

impl<T: AsyncRead + AsyncWrite + Send + Unpin> SwitchedStream for T {}

/// The client's connection, which resolves once the `101` answer has been written to it.
pub type ClientConnection =
    Pin<Box<dyn Future<Output = std::io::Result<Box<dyn SwitchedStream>>> + Send>>;

#[derive(Default)]
struct State {
    client: Option<ClientConnection>,
    protocol: Option<String>,
}

/// Where a listener leaves the client's connection for a plane that switches its protocol.
///
/// Cloned into the request's extensions; every clone is the same switch. The connection is
/// taken once.
#[derive(Clone)]
pub struct ProtocolSwitch(Arc<Mutex<State>>);

impl std::fmt::Debug for ProtocolSwitch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProtocolSwitch").finish_non_exhaustive()
    }
}

impl ProtocolSwitch {
    /// A switch over the connection `client` resolves to.
    pub fn new<F, S>(client: F) -> Self
    where
        F: Future<Output = std::io::Result<S>> + Send + 'static,
        S: SwitchedStream + 'static,
    {
        let client: ClientConnection = Box::pin(async move {
            let stream = client.await?;
            Ok(Box::new(stream) as Box<dyn SwitchedStream>)
        });
        Self(Arc::new(Mutex::new(State {
            client: Some(client),
            protocol: None,
        })))
    }

    /// Take the client's connection for a switch to `protocol`. `None` when it was already
    /// taken.
    pub fn accept(&self, protocol: &str) -> Option<ClientConnection> {
        let mut state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let client = state.client.take()?;
        state.protocol = Some(protocol.to_string());
        Some(client)
    }

    /// The protocol a plane switched the connection to, if one did.
    pub fn accepted(&self) -> Option<String> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .protocol
            .clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_connection_is_handed_over_once_and_the_protocol_is_remembered() {
        let (_client, listener_side) = tokio::io::duplex(8);
        let switch = ProtocolSwitch::new(async move { Ok(listener_side) });
        assert_eq!(switch.accepted(), None, "no plane has switched the connection yet");

        let carried = switch.clone();
        assert!(carried.accept("websocket").is_some(), "the first plane to ask gets the connection");

        assert_eq!(switch.accepted().as_deref(), Some("websocket"));
        assert!(switch.accept("websocket").is_none(), "a connection is handed over once");
    }
}
