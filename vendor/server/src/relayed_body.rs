// SPDX-License-Identifier: CC0-1.0

//! fork: a response body a service relays as it arrives, rather than one it holds whole.
//!
//! The [`crate::EmulatorService`] contract answers a request with a buffered
//! `http::Response<Bytes>`, which suits every AWS API: an operation produces its answer and
//! then it is written. A load balancer is different. It relays what its target sends while
//! the target is still sending it, so a Server-Sent Events stream or a long download reaches
//! the client a piece at a time.
//!
//! So a request that arrives on a network listener carries a [`ResponseBodySlot`] among its
//! extensions. A plane that relays a body puts it in the slot and answers with the status and
//! headers alone; the listener that made the slot then writes the response with that body.
//! A request made in process carries no slot, and the plane answers it buffered, as before,
//! unless its caller asked for the body as it arrives with [`RelayedResponseWanted`]: then
//! [`crate::http_connector_call`] gives the request a slot of its own and hands the caller a
//! response whose body streams what the plane left there. A CloudFront edge asks for that on
//! an origin request it relays to a viewer, so an origin served in process (a load balancer)
//! reaches the viewer a piece at a time too.

use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};

use bytes::Bytes;
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt as _, Full};
use hyper::body::{Body, Frame, SizeHint};

/// The error a relayed body ends with when its source fails part of the way through.
pub type RelayError = Box<dyn std::error::Error + Send + Sync>;

/// A response body written as its frames arrive.
pub struct RelayedBody(UnsyncBoxBody<Bytes, RelayError>);

impl RelayedBody {
    /// A body that relays `body`'s frames.
    pub fn new<B>(body: B) -> Self
    where
        B: Body<Data = Bytes> + Send + 'static,
        B::Error: Into<RelayError>,
    {
        Self(body.map_err(Into::into).boxed_unsync())
    }
}

impl From<Bytes> for RelayedBody {
    /// A body that is already whole.
    fn from(bytes: Bytes) -> Self {
        Self::new(Full::new(bytes))
    }
}

impl std::fmt::Debug for RelayedBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RelayedBody").finish_non_exhaustive()
    }
}

impl Body for RelayedBody {
    type Data = Bytes;
    type Error = RelayError;

    fn poll_frame(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, RelayError>>> {
        Pin::new(&mut self.get_mut().0).poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.0.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.0.size_hint()
    }
}

impl RelayedBody {
    /// This body as the body of an in-process response, which the SDK's transport requires to
    /// be `Sync` as well as `Send`.
    pub(crate) fn into_sdk_body(self) -> aws_smithy_types::body::SdkBody {
        aws_smithy_types::body::SdkBody::from_body_1_x(SharedRelayedBody(Mutex::new(self)))
    }
}

/// fork: a [`RelayedBody`] behind a mutex, which makes it `Sync`. It is only ever polled
/// through `&mut`, so the mutex is never contended: [`Mutex::get_mut`] takes no lock.
struct SharedRelayedBody(Mutex<RelayedBody>);

impl Body for SharedRelayedBody {
    type Data = Bytes;
    type Error = RelayError;

    fn poll_frame(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, RelayError>>> {
        let body = self.get_mut().0.get_mut().unwrap_or_else(PoisonError::into_inner);
        Pin::new(body).poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).size_hint()
    }
}

/// fork: the marker an in-process caller puts among its request's extensions to be handed
/// the callee's body as it arrives, rather than whole.
///
/// [`crate::http_connector_call`] gives a request that carries it a [`ResponseBodySlot`]. A
/// callee that relays its body leaves it there, and the caller gets a response whose body
/// streams it, with no `Content-Length`. A callee that relays nothing answers whole, as
/// before. A request without the marker is always answered whole, so a caller that reads the
/// body with `SdkBody::bytes` keeps reading all of it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RelayedResponseWanted;

/// Where a plane leaves the body it relays for the listener that received the request.
///
/// Cloned into the request's extensions; every clone is the same slot. The body is taken
/// once.
#[derive(Clone, Default)]
pub struct ResponseBodySlot(Arc<Mutex<Option<RelayedBody>>>);

impl std::fmt::Debug for ResponseBodySlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResponseBodySlot").finish_non_exhaustive()
    }
}

impl ResponseBodySlot {
    /// Leave `body` for the listener to write. A body left earlier is replaced.
    pub fn put(&self, body: RelayedBody) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = Some(body);
    }

    /// The body a plane left, if any, taken out of the slot.
    pub fn take(&self) -> Option<RelayedBody> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_body_left_in_a_clone_is_taken_once_from_the_slot() {
        let slot = ResponseBodySlot::default();
        let clone = slot.clone();

        clone.put(RelayedBody::from(Bytes::from_static(b"relayed")));

        assert!(slot.take().is_some(), "the body left in a clone is in the slot");
        assert!(slot.take().is_none(), "the body is taken once");
    }
}
