# Craftec — P2P Cloud Infrastructure

**Craftec is the P2P equivalent of what Google, Microsoft, and Amazon built in data centres.**

Where hyperscalers centralise storage, compute, and database infrastructure behind proprietary walls, Craftec distributes those same primitives across a peer-to-peer network of commodity nodes — no single point of control, no single point of failure, no single owner.

---

## Architecture

```
┌──────────────────────────────────────────────────────────────┐
│                    Craftec Node (craftec-node)                │
│                                                              │
│  ┌──────────────────────────────────────────────────────┐   │
│  │                  Application Layer                    │   │
│  │        craftec-sql (CraftSQL distributed DB)          │   │
│  │        craftec-com (CraftCOM WASM compute)            │   │
│  └────────────────────────┬─────────────────────────────┘   │
│                            │                                  │
│  ┌──────────────────────────────────────────────────────┐   │
│  │                  Storage Layer                        │   │
│  │   craftec-obj (CraftOBJ content-addressed store)      │   │
│  │   craftec-rlnc (RLNC erasure coding over GF(2^8))     │   │
│  │   craftec-vfs  (CID-VFS: SQLite → CraftOBJ bridge)    │   │
│  └────────────────────────┬─────────────────────────────┘   │
│                            │                                  │
│  ┌──────────────────────────────────────────────────────┐   │
│  │                  Network Layer                        │   │
│  │   craftec-net  (QUIC via iroh, SWIM membership)       │   │
│  │   craftec-health (redundancy scanning + repair)       │   │
│  └────────────────────────┬─────────────────────────────┘   │
│                            │                                  │
│  ┌──────────────────────────────────────────────────────┐   │
│  │                  Foundation Layer                     │   │
│  │   craftec-types  (shared types, config, events)       │   │
│  │   craftec-crypto (Ed25519 identity, BLAKE3, HoMAC)    │   │
│  └──────────────────────────────────────────────────────┘   │
└──────────────────────────────────────────────────────────────┘
```

---

## Crate Structure

| Crate | Type | Responsibility |
|---|---|---|
| `craftec-types` | lib | Shared types: `Cid`, `NodeId`, `NodeConfig`, `WireMessage`, `Event`, `CodedPiece` |
| `craftec-crypto` | lib | Ed25519 `KeyStore`, BLAKE3 hashing helpers, HoMAC (homomorphic MAC for RLNC) |
| `craftec-obj` | lib | CraftOBJ: content-addressed local object store (filesystem + LRU + Bloom filter) |
| `craftec-rlnc` | lib | Random-linear network coding (RLNC) over GF(2^8): encoder, decoder, recoder |
| `craftec-vfs` | lib | CID-VFS: maps SQLite page I/O to CraftOBJ, enabling snapshot isolation |
| `craftec-sql` | lib | CraftSQL: distributed SQLite — CID-VFS integration, commit anchoring, RPC writes |
| `craftec-net` | lib | P2P networking: iroh/QUIC endpoint, SWIM membership, DHT, connection pool |
| `craftec-health` | lib | CID health scanning, piece availability tracking, repair coordination |
| `craftec-com` | lib | CraftCOM: Wasmtime WASM runtime, agent execution, fuel-bounded scheduling |
| `craftec-node` | bin | Entry point: composes all crates into a running Craftec node (`craftec` binary) |

---

## Quick Start

### Prerequisites

- Rust 1.85 or later (`rustup update stable`)
- A Unix-like system or Windows (signal handling is platform-aware)

### Build

```sh
# Clone the repository
git clone https://github.com/onlyabrak/craftec
cd craftec

# Debug build (fast compile, slow runtime)
cargo build

# Release build (production — LTO + symbol stripping)
cargo build --release
```

The binary is placed at:
- Debug: `target/debug/craftec`
- Release: `target/release/craftec`

### Run a Node

```sh
# Run with default configuration (creates craftec.json on first run)
./target/debug/craftec

# Run with debug logging
RUST_LOG=debug ./target/debug/craftec

# Run with per-crate log levels
RUST_LOG=craftec_net=trace,craftec_health=debug,info ./target/debug/craftec
```

On first start, a `craftec.json` configuration file is created in the working directory. Edit it to configure:

```json
{
  "data_dir": "data",
  "listen_port": 4433,
  "bootstrap_peers": ["peer1.example.com:4433", "peer2.example.com:4433"],
  "max_connections": 256,
  "max_disk_usage_bytes": 10737418240,
  "health_scan_interval_secs": 3600,
  "rlnc_k": 32,
  "page_size": 16384,
  "log_level": "info"
}
```

### Run Tests

```sh
# All tests across the entire workspace
cargo test --workspace

# Tests for a specific crate
cargo test -p craftec-rlnc

# Tests with log output
RUST_LOG=debug cargo test --workspace -- --nocapture
```

---

## Key Design Decisions

| Concern | Choice | Rationale |
|---|---|---|
| **Transport** | QUIC via [iroh](https://iroh.computer) 0.96 | Built-in NAT traversal, relay fallback, multiplexed streams, TLS 1.3 |
| **Membership** | SWIM (Scalable Weakly-consistent Infection-style) | O(1) messages per node per period, O(log N) dissemination latency |
| **Identity** | Ed25519 (ed25519-dalek 2) | NodeId = 32-byte compressed public key (same convention as iroh) |
| **Serialization** | postcard (wire), serde_json (config) | postcard: compact binary, zero-copy, no-std compatible; JSON: human-readable config |
| **Erasure coding** | RLNC over GF(2^8) | Random linear codes eliminate coordination overhead vs. Reed-Solomon |
| **Database** | libsql (SQLite-compatible) | Full SQL, WASM-deployable, CID-VFS bridges pages to CraftOBJ |
| **Hashing** | BLAKE3 | 1 GB/s on commodity hardware, parallel Merkle tree, 256-bit security |
| **Content routing** | DHT + Bloom filters | DhtProviders maps CIDs to holder nodes; Bloom filters eliminate negative lookups |

---

## Node Roles

Craftec nodes are symmetric — every node runs the same binary. Role behaviour is determined by configuration and network topology:

| Role | Description | Typical config |
|---|---|---|
| **Storage node** | Stores and serves coded pieces; participates in SWIM; runs health scanner | High `max_disk_usage_bytes`, stable uptime |
| **Client node** | Issues reads/writes; does not serve pieces to others | Low `max_disk_usage_bytes`, short-lived |
| **RPC node** | Bridges external SQL clients to the CraftSQL layer | Exposed port, no local storage required |

---

## Startup Sequence (Join Path)

The node follows a deterministic initialisation order defined in Technical Foundation v3.3, Section 57:

```
Step  1  Create / verify data directory
Step  2  Write node.lock sentinel (detect dirty shutdown)
Step  3  KeyStore: load or generate Ed25519 keypair
Step  4  ContentAddressedStore (CraftOBJ)
Step  5  RlncEngine
Step  6  CidVfs
Step  7  ComRuntime (Wasmtime)
Step  8  EventBus (broadcast channels)
Step  9  CraftecEndpoint (iroh QUIC)
Step 10  SwimMembership (from endpoint)
Step 11  HealthScanner + PieceTracker
Step 12  ProgramScheduler
```

After initialisation: bootstrap → accept loop → SWIM loop → health loop → event loop → wait for signal → graceful shutdown.

---

## Project Layout

```
craftec/
├── Cargo.toml                  # Workspace manifest
├── Cargo.lock                  # Locked dependency versions (committed for binary crate)
├── README.md                   # This file
├── LICENSE-MIT
├── LICENSE-APACHE
├── .gitignore
├── .github/
│   └── workflows/
│       └── ci.yml              # GitHub Actions CI pipeline
├── docs/
│   ├── TESTING.md              # Testing guide
│   └── DEVELOPMENT.md          # Development process guide
└── crates/
    ├── craftec-types/
    ├── craftec-crypto/
    ├── craftec-obj/
    ├── craftec-rlnc/
    ├── craftec-vfs/
    ├── craftec-sql/
    ├── craftec-net/
    ├── craftec-health/
    ├── craftec-com/
    └── craftec-node/
```

---

## License

Licensed under either of:

- MIT license ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)
- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in Craftec by you shall be dual-licensed as above, without any additional terms or conditions.
