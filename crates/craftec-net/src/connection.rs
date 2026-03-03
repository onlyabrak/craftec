//! [`ConnectionHandler`] — application-layer trait for handling incoming Craftec messages.
//!
//! Implementors receive deserialized [`WireMessage`]s from authenticated peers and may
//! optionally return a reply message.  The handler is invoked from `CraftecEndpoint`'s
//! `accept_loop` for every message received on the `craftec/0.1` ALPN.
//!
//! # Design note
//!
//! `async_trait` is intentionally **not** added as a dependency to keep the dependency
//! footprint small.  The trait uses `Pin<Box<dyn Future>>` for async methods instead.
//! Implementors that need ergonomic async syntax should use the `async fn` inside an
//! `impl` block with explicit `Box::pin(async move { … })` in the trait method body,
//! or wrap the impl with a newtype that boxes the future.

use std::future::Future;
use std::pin::Pin;

use craftec_types::{NodeId, WireMessage};

/// A boxed, heap-allocated future returned by [`ConnectionHandler::handle_message`].
///
/// Using a type alias avoids repeating the verbose type signature at every call site.
pub type HandlerFuture = Pin<Box<dyn Future<Output = Option<WireMessage>> + Send + 'static>>;

/// Trait for processing incoming Craftec wire messages.
///
/// Implementors are placed behind an `Arc` and shared with the `accept_loop`.
/// Every method **must** be non-blocking (or correctly async) — blocking inside
/// `handle_message` will stall the accept task.
///
/// # Example
///
/// ```rust,ignore
/// struct EchoHandler;
///
/// impl ConnectionHandler for EchoHandler {
///     fn handle_message(&self, from: NodeId, msg: WireMessage) -> HandlerFuture {
///         Box::pin(async move {
///             tracing::info!(peer = %from, "echo: {:?}", msg);
///             Some(msg) // echo the message back
///         })
///     }
/// }
/// ```
pub trait ConnectionHandler: Send + Sync + 'static {
    /// Process a single incoming message from `from` and optionally return a reply.
    ///
    /// The returned future is polled by the accept loop.  Returning `None` means no reply
    /// is sent.  The future must be `Send + 'static` so it can be spawned on Tokio.
    fn handle_message(&self, from: NodeId, msg: WireMessage) -> HandlerFuture;
}

/// A no-op handler that discards all messages and never replies.
///
/// Useful as a placeholder during testing or when a sub-protocol does not require responses.
pub struct NullHandler;

impl ConnectionHandler for NullHandler {
    fn handle_message(&self, from: NodeId, _msg: WireMessage) -> HandlerFuture {
        tracing::trace!(peer = %from, "NullHandler: discarding message");
        Box::pin(async move { None })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Confirm that `NullHandler` returns `None` without panicking.
    #[tokio::test]
    async fn null_handler_returns_none() {
        use craftec_types::{NodeId, WireMessage};

        // We can only construct a WireMessage if the type is available; use a minimal stub.
        // This test primarily verifies the trait object compiles and runs.
        let handler = NullHandler;
        let node_id = NodeId::generate();
        let msg = WireMessage::Ping { nonce: 0 };
        let result = handler.handle_message(node_id, msg).await;
        assert!(result.is_none());
    }
}
