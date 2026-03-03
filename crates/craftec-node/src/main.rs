//! Craftec Node — P2P cloud infrastructure node.
//!
//! Starts a Craftec node that participates in the distributed storage,
//! compute, and database network.
//!
//! # Startup sequence
//!
//! 1. **Tracing** — configure `tracing-subscriber` from `RUST_LOG` or default `"info"`.
//! 2. **Configuration** — load `craftec.json` from the working directory, or write a
//!    default config file and use that.
//! 3. **Node** — construct [`node::CraftecNode`], which initialises all subsystems
//!    in dependency order (see [`node`] module documentation).
//! 4. **Run** — hand control to [`node::CraftecNode::run`], which bootstraps into
//!    the network and blocks until a shutdown signal is received.
//!
//! # Configuration
//!
//! On first run a `craftec.json` file is written to the current working directory
//! with sensible defaults.  Edit it to customise data directory, port, bootstrap
//! peers, and other parameters before subsequent runs.
//!
//! # Environment variables
//!
//! | Variable | Effect |
//! |---|---|
//! | `RUST_LOG` | Log level filter, e.g. `info`, `craftec=debug`, `trace` |
//! | `RUST_BACKTRACE` | Set to `1` or `full` to enable panic backtraces |

use anyhow::Result;
use tracing_subscriber::{EnvFilter, fmt};

mod event_bus;
mod node;
mod shutdown;

#[tokio::main]
async fn main() -> Result<()> {
    // 1. Initialize tracing
    init_tracing();
    tracing::info!("Craftec node starting...");

    // 2. Load configuration
    let config = load_or_create_config()?;
    tracing::info!(
        data_dir = %config.data_dir.display(),
        port = config.listen_port,
        "Configuration loaded"
    );

    // 3. Initialize node
    let node = node::CraftecNode::new(config).await?;

    // 4. Run until shutdown signal
    node.run().await?;

    tracing::info!("Craftec node shutdown complete");
    Ok(())
}

/// Configure `tracing-subscriber` using `RUST_LOG` environment variable.
///
/// Falls back to `"info"` if `RUST_LOG` is unset or unparseable.
/// Output format: human-readable with targets, thread IDs, file, and line number.
fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info"));

    fmt()
        .with_env_filter(filter)
        .with_target(true)
        .with_thread_ids(true)
        .with_file(true)
        .with_line_number(true)
        .init();
}

/// Load node configuration from `craftec.json`, or create a default if absent.
///
/// On first run, writes `craftec.json` with default values to the current
/// working directory and logs the path.  On subsequent runs, loads and returns
/// the existing file.
///
/// # Errors
/// Returns an error if the config file exists but cannot be read or parsed,
/// or if the default config cannot be written to disk.
fn load_or_create_config() -> Result<craftec_types::config::NodeConfig> {
    let config_path = std::path::PathBuf::from("craftec.json");
    if config_path.exists() {
        tracing::info!(path = %config_path.display(), "Loading configuration from file");
        Ok(craftec_types::config::NodeConfig::load(&config_path)?)
    } else {
        tracing::info!("No configuration file found, using defaults");
        let config = craftec_types::config::NodeConfig::default();
        config.save(&config_path)?;
        tracing::info!(path = %config_path.display(), "Default configuration written");
        Ok(config)
    }
}
