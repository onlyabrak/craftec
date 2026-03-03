//! Internal publish/subscribe event bus for Craftec subsystem communication.
//!
//! [`EventBus`] wraps a `tokio::sync::broadcast` channel carrying [`Event`]
//! values, providing a single-producer, multi-consumer fan-out bus.  All
//! subsystems (health scanner, SWIM membership, compute runtime, etc.) receive
//! the same events independently via their own [`broadcast::Receiver`].
//!
//! ## Usage
//!
//! ```rust,ignore
//! let bus = EventBus::new(256);
//!
//! // Subscribe before publishing so no events are missed.
//! let mut rx = bus.subscribe();
//!
//! bus.publish(Event::PeerConnected { node_id });
//!
//! match rx.recv().await? {
//!     Event::PeerConnected { node_id } => { /* handle */ }
//!     _ => {}
//! }
//! ```
//!
//! ## Slow receivers
//!
//! The channel is bounded by `capacity`.  If a receiver falls too far behind
//! the sender, it will receive a
//! [`RecvError::Lagged`](tokio::sync::broadcast::error::RecvError::Lagged)
//! error.  Use `craftec_types::event` capacity constants for recommended sizes.
//!
//! ## Thread safety
//!
//! `EventBus` is `Clone + Send + Sync`.  The inner `broadcast::Sender` is
//! reference-counted by Tokio, so all clones share the same channel.

use tokio::sync::broadcast;
use craftec_types::event::Event;

/// Bounded broadcast event bus.
///
/// Wrap in `Arc` to share between tasks, or clone the `Sender` directly.
pub struct EventBus {
    sender: broadcast::Sender<Event>,
}

impl EventBus {
    /// Create a new `EventBus` with the given channel `capacity`.
    ///
    /// Choose `capacity` carefully: too small and slow receivers will lag
    /// and lose events; too large wastes memory.  See
    /// [`craftec_types::event`] for per-event-type recommendations.
    pub fn new(capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(capacity);
        tracing::info!(capacity = capacity, "Event bus initialized");
        Self { sender }
    }

    /// Publish an [`Event`] to all current subscribers.
    ///
    /// If there are no subscribers the event is silently dropped and a
    /// `WARN`-level trace is emitted.  This is intentional: subsystems may
    /// not be subscribed yet at startup.
    ///
    /// Errors from [`broadcast::Sender::send`] (i.e. no receivers) are
    /// handled gracefully — they do **not** cause a panic.
    pub fn publish(&self, event: Event) {
        // Format the variant name before moving `event` into send().
        let event_name = format!("{:?}", event);
        match self.sender.send(event) {
            Ok(receivers) => {
                tracing::debug!(
                    event = %event_name,
                    receivers = receivers,
                    "Event published"
                );
            }
            Err(_) => {
                tracing::warn!(
                    event = %event_name,
                    "Event published but no receivers"
                );
            }
        }
    }

    /// Subscribe to the event bus, returning a new [`broadcast::Receiver`].
    ///
    /// The receiver will only see events published *after* this call.
    /// Subscribe before any events you care about are published.
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.sender.subscribe()
    }

    /// Return the number of active receivers on this bus.
    ///
    /// Useful for diagnostics and health checks.
    pub fn receiver_count(&self) -> usize {
        self.sender.receiver_count()
    }
}
