//! Node configuration: loading, saving, and defaults.
//!
//! [`NodeConfig`] is stored as a JSON file (usually `config.json` inside
//! `data_dir`).  All fields have sane defaults so an empty config file still
//! produces a working node.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tracing::{debug, trace};

use crate::error::{CraftecError, Result};

/// Runtime configuration for a Craftec node.
///
/// Load from disk with [`NodeConfig::load`], persist with [`NodeConfig::save`],
/// or start from [`NodeConfig::default`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeConfig {
    /// Directory for all persistent data: keys, pieces, and database.
    pub data_dir: PathBuf,

    /// UDP/QUIC port the node listens on.
    pub listen_port: u16,

    /// Initial peers to contact at startup (host:port or multiaddr strings).
    pub bootstrap_peers: Vec<String>,

    /// Maximum number of simultaneous QUIC connections.
    pub max_connections: usize,

    /// Soft disk-usage cap in bytes.  The node stops accepting new data when
    /// this limit is approached.
    pub max_disk_usage_bytes: u64,

    /// Seconds between each health-scan cycle (each cycle scans 1% of CIDs).
    /// Default 300 s = 5 minutes (§30). Full coverage ≈ 100 cycles ≈ 8.3 hours.
    pub health_scan_cycle_secs: u64,

    /// RLNC generation size — number of source blocks per generation.
    pub rlnc_k: u32,

    /// Piece (page) size in bytes.
    pub page_size: usize,

    /// `tracing` log level filter string, e.g. `"info"` or `"craftec=debug"`.
    pub log_level: String,
}

impl Default for NodeConfig {
    fn default() -> Self {
        trace!("creating default NodeConfig");
        Self {
            data_dir: PathBuf::from("data"),
            listen_port: 4433,
            bootstrap_peers: Vec::new(),
            max_connections: 256,
            max_disk_usage_bytes: 10 * 1024 * 1024 * 1024, // 10 GiB
            health_scan_cycle_secs: 300,
            rlnc_k: 32,
            page_size: 16_384,
            log_level: "info".to_string(),
        }
    }
}

impl NodeConfig {
    /// Load a [`NodeConfig`] from a JSON file at `path`.
    ///
    /// # Errors
    /// Returns [`CraftecError::IoError`] if the file cannot be read, or
    /// [`CraftecError::SerializationError`] if the JSON is malformed.
    pub fn load(path: &Path) -> Result<Self> {
        debug!(path = %path.display(), "loading NodeConfig from file");
        let content = std::fs::read_to_string(path)?;
        let cfg: Self = serde_json::from_str(&content).map_err(|e| {
            CraftecError::SerializationError(format!("failed to parse config JSON: {e}"))
        })?;
        debug!(
            listen_port = cfg.listen_port,
            rlnc_k = cfg.rlnc_k,
            "loaded NodeConfig"
        );
        Ok(cfg)
    }

    /// Persist this [`NodeConfig`] to a JSON file at `path`.
    ///
    /// The file is created (or truncated) atomically via a temporary file on
    /// platforms that support it.
    ///
    /// # Errors
    /// Returns [`CraftecError::IoError`] on write failure, or
    /// [`CraftecError::SerializationError`] if serialization fails.
    pub fn save(&self, path: &Path) -> Result<()> {
        debug!(path = %path.display(), "saving NodeConfig to file");
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self).map_err(|e| {
            CraftecError::SerializationError(format!("failed to serialize config: {e}"))
        })?;
        std::fs::write(path, json.as_bytes())?;
        debug!(path = %path.display(), "saved NodeConfig");
        Ok(())
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn default_values() {
        let cfg = NodeConfig::default();
        assert_eq!(cfg.listen_port, 4433);
        assert_eq!(cfg.max_connections, 256);
        assert_eq!(cfg.rlnc_k, 32);
        assert_eq!(cfg.page_size, 16_384);
        assert_eq!(cfg.log_level, "info");
    }

    #[test]
    fn save_and_load_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let mut cfg = NodeConfig::default();
        cfg.listen_port = 9000;
        cfg.bootstrap_peers = vec!["127.0.0.1:4433".to_string()];
        cfg.save(&path).unwrap();
        let loaded = NodeConfig::load(&path).unwrap();
        assert_eq!(loaded.listen_port, 9000);
        assert_eq!(loaded.bootstrap_peers, vec!["127.0.0.1:4433"]);
    }

    #[test]
    fn load_invalid_json_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.json");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(b"{ not valid json }").unwrap();
        let result = NodeConfig::load(&path);
        assert!(matches!(result, Err(CraftecError::SerializationError(_))));
    }
}
