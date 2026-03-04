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

mod cli;
mod event_bus;
mod handler;
mod node;
mod pending;
mod piece_store;
mod rpc;
mod shutdown;

#[tokio::main]
async fn main() -> Result<()> {
    // Check if this is a CLI subcommand invocation (not a node start).
    let args: Vec<String> = std::env::args().skip(1).collect();
    if !args.is_empty() {
        // CLI mode — no tracing init (keep stdout clean for machine parsing).
        return cli::run_cli(&args).await;
    }

    // ── Node mode (original behavior) ──────────────────────────────────────

    // 1. Initialize tracing
    init_tracing();
    tracing::info!("Craftec node starting...");

    // 2. Load configuration (with Docker env var overrides)
    let config = load_or_create_config()?;
    let config = apply_env_overrides(config);
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
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    fmt()
        .with_env_filter(filter)
        .with_target(true)
        .with_thread_ids(true)
        .with_file(true)
        .with_line_number(true)
        .init();
}

/// Apply environment variable overrides to the configuration.
///
/// Supported variables:
/// - `CRAFTEC_DATA_DIR` → overrides `config.data_dir`
/// - `CRAFTEC_LISTEN_PORT` → overrides `config.listen_port`
/// - `CRAFTEC_BOOTSTRAP_PEERS` → overrides `config.bootstrap_peers` (comma-separated)
fn apply_env_overrides(
    mut config: craftec_types::config::NodeConfig,
) -> craftec_types::config::NodeConfig {
    if let Ok(dir) = std::env::var("CRAFTEC_DATA_DIR") {
        tracing::info!(data_dir = %dir, "Env override: CRAFTEC_DATA_DIR");
        config.data_dir = std::path::PathBuf::from(dir);
    }
    if let Ok(port_str) = std::env::var("CRAFTEC_LISTEN_PORT") {
        if let Ok(port) = port_str.parse::<u16>() {
            tracing::info!(port, "Env override: CRAFTEC_LISTEN_PORT");
            config.listen_port = port;
        } else {
            tracing::warn!(value = %port_str, "CRAFTEC_LISTEN_PORT: invalid port number");
        }
    }
    if let Ok(peers_str) = std::env::var("CRAFTEC_BOOTSTRAP_PEERS") {
        let peers: Vec<String> = peers_str
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        tracing::info!(count = peers.len(), "Env override: CRAFTEC_BOOTSTRAP_PEERS");
        config.bootstrap_peers = peers;
    }
    if let Ok(val) = std::env::var("CRAFTEC_HEALTH_SCAN_CYCLE_SECS")
        && let Ok(secs) = val.parse::<u64>()
    {
        tracing::info!(secs, "Env override: CRAFTEC_HEALTH_SCAN_CYCLE_SECS");
        config.health_scan_cycle_secs = secs;
    }
    config
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
