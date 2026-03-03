//! Error types for the `craftec-net` networking layer.
//!
//! All fallible operations in this crate return [`NetError`] or the
//! crate-local [`Result`] alias.

use thiserror::Error;

/// Errors that can arise within the Craftec networking layer.
#[derive(Debug, Error)]
pub enum NetError {
    /// A connection to a peer could not be established.
    ///
    /// This covers both direct QUIC failures and relay-path failures.
    #[error("connection failed to peer {peer}: {reason}")]
    ConnectionFailed { peer: String, reason: String },

    /// An operation timed out before completing.
    #[error("network operation timed out after {millis}ms")]
    Timeout { millis: u64 },

    /// The requested peer is not known in the membership table or connection pool.
    #[error("peer not found: {0}")]
    PeerNotFound(String),

    /// A protocol-level error: malformed message, unexpected ALPN, or state machine violation.
    #[error("protocol error: {0}")]
    ProtocolError(String),

    /// Bootstrap failed — could not reach any of the configured bootstrap peers.
    #[error("bootstrap failed: {0}")]
    BootstrapFailed(String),

    /// Serialization or deserialization of a wire message failed.
    #[error("serialization error: {0}")]
    SerializationError(String),

    /// An I/O error from an underlying stream or socket.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// Crate-local result alias backed by [`NetError`].
pub type Result<T, E = NetError> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_failed_display() {
        let err = NetError::ConnectionFailed {
            peer: "abc123".into(),
            reason: "unreachable".into(),
        };
        assert!(err.to_string().contains("abc123"));
        assert!(err.to_string().contains("unreachable"));
    }

    #[test]
    fn timeout_display() {
        let err = NetError::Timeout { millis: 5000 };
        assert!(err.to_string().contains("5000"));
    }

    #[test]
    fn peer_not_found_display() {
        let err = NetError::PeerNotFound("xyz".into());
        assert!(err.to_string().contains("xyz"));
    }

    #[test]
    fn protocol_error_display() {
        let err = NetError::ProtocolError("unexpected ALPN".into());
        assert!(err.to_string().contains("unexpected ALPN"));
    }

    #[test]
    fn bootstrap_failed_display() {
        let err = NetError::BootstrapFailed("no peers reachable".into());
        assert!(err.to_string().contains("no peers reachable"));
    }
}
