//! Graceful shutdown signal handling for the Craftec node.
//!
//! Listens for OS-level termination signals and provides a unified async
//! future that resolves when the process should begin shutting down.
//!
//! ## Supported signals
//!
//! | Platform | Signals handled |
//! |---|---|
//! | Unix (Linux, macOS) | `SIGINT` (Ctrl+C), `SIGTERM` |
//! | Windows | `Ctrl+C` only |
//!
//! The first signal to arrive wins; the future returns without consuming
//! subsequent signals.

use tokio::signal;

/// Wait asynchronously for a shutdown signal (`Ctrl+C` or `SIGTERM`).
///
/// Returns as soon as the first recognised shutdown signal is received.
/// The caller is responsible for propagating the shutdown through the rest
/// of the application (e.g., by sending on a `broadcast::Sender<()>`).
///
/// # Panics
/// Panics if the OS signal handler cannot be installed.  This is expected
/// to be fatal — the node cannot safely run without shutdown support.
///
/// # Example
/// ```rust,ignore
/// shutdown::wait_for_shutdown().await;
/// // Begin graceful teardown…
/// ```
pub async fn wait_for_shutdown() {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("Failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("Failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {
            tracing::info!("Received Ctrl+C, initiating shutdown...");
        }
        _ = terminate => {
            tracing::info!("Received SIGTERM, initiating shutdown...");
        }
    }
}
