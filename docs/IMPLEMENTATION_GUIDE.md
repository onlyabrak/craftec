# CRAFTEC IMPLEMENTATION GUIDE
## Complete Build Instructions — From Zero to Running Network

> **Audience:** Experienced Rust developer who has read the v3.3 technical foundation PDF.
> **Purpose:** Every section provides actionable implementation guidance — exact types, exact function signatures, exact behavior. No padding.
> **Status convention:** DONE = fully implemented and tested; STUBBED = code present but behavior faked; MISSING = not written at all.

---

## PART 0: PROJECT OVERVIEW

### What Exists Today

| Crate | Status | What Works | What Doesn't |
|---|---|---|---|
| `craftec-types` | **DONE** | All types, serde, CID, config, events, wire protocol structs | — |
| `craftec-crypto` | **MOSTLY DONE** | BLAKE3 hashing, Ed25519 sign/verify, KeyStore | `combine_tags()` uses XOR placeholder, not Catalano-Fiore |
| `craftec-rlnc` | **MOSTLY DONE** | GF(2^8) tables, Encoder, Decoder (Gaussian elimination), Engine concurrency | Recoder HomMAC combination is a structural placeholder |
| `craftec-obj` | **DONE** | CAS store, bloom filter, LRU cache, sharded dirs, integrity verification | — |
| `craftec-vfs` | **DONE** | Page cache, page index, snapshot isolation, commit pipeline, CidVfs | Not wired to real SQLite VFS yet |
| `craftec-sql` | **STUBBED** | Ownership checks, CAS versioning, RPC write signature verification | `execute()` fakes SQL as raw page bytes; `query()` always returns `Vec::new()` |
| `craftec-net` | **MOSTLY DONE** | SWIM state machine, DHT in-memory, ConnectionPool, iroh Endpoint scaffolding | SWIM probes not dispatched; handler replies discarded; DHT has no gossip/TTL |
| `craftec-health` | **PARTIALLY STUBBED** | Natural selection coordinator, PieceTracker, HealthScanner scan logic | Repair uses network fetch instead of local recode; parallel repair (top-N election) not implemented; distribution priority (1-piece holders first) missing |
| `craftec-com` | **PARTIALLY STUBBED** | Wasmtime ComRuntime executes real WASM; agent lifecycle state machine; `craft_log` works | `craft_store_get/put`, `craft_sql_query`, `craft_sign` all stub-return 0; scheduler sets Running state but never executes |
| `craftec-node` | **STRUCTURALLY COMPLETE** | Starts all subsystems; graceful shutdown; SIGINT handling | `LoggingHandler` discards all inbound messages; event bus only logs; no SQL DB init; no SWIM probe dispatch |
| `craftec-tests` | **PARTIAL** | 10 integration tests: RLNC, SWIM in-process, wire round-trips, E2E pipeline | No real networking; no SQL tests; no health repair integration |

### Crate Dependency Graph

```
craftec-types          (no internal deps)
    │
    ├──► craftec-crypto       → craftec-types
    │
    ├──► craftec-rlnc         → craftec-types, craftec-crypto
    │
    ├──► craftec-obj          → craftec-types, craftec-crypto
    │       │
    │       └──► craftec-vfs  → craftec-types, craftec-crypto, craftec-obj
    │               │
    │               └──► craftec-sql → craftec-types, craftec-crypto, craftec-obj, craftec-vfs
    │
    ├──► craftec-net          → craftec-types, craftec-crypto
    │
    ├──► craftec-health       → craftec-types, craftec-crypto, craftec-obj, craftec-rlnc, craftec-net
    │
    ├──► craftec-com          → craftec-types, craftec-crypto, craftec-obj, craftec-sql
    │
    └──► craftec-node         → ALL 9 library crates above
             │
             └──► craftec-tests → craftec-types, craftec-crypto, craftec-rlnc (integration only)
```

**Key constraint:** `craftec-sql` does NOT depend on `craftec-net`. SQL cannot route CIDs — CraftOBJ handles all content discovery via DHT. This prevents circular dependencies.

### Build Order

Build crates in this exact order to avoid compilation errors:

1. `craftec-types` — zero deps; defines everything else
2. `craftec-crypto` — depends only on types
3. `craftec-rlnc` — depends on types + crypto (GF(2^8) needs hommac)
4. `craftec-obj` — depends on types + crypto (CAS store)
5. `craftec-vfs` — depends on types + crypto + obj (page index over CAS)
6. `craftec-sql` — depends on types + crypto + obj + vfs (SQL over CAS pages)
7. `craftec-net` — depends on types + crypto only (networking is orthogonal to storage)
8. `craftec-health` — depends on types + crypto + obj + rlnc + net (repair needs all of storage and network)
9. `craftec-com` — depends on types + crypto + obj + sql (WASM agents call into storage + SQL)
10. `craftec-node` — depends on all 9 above (assembly binary)

`craftec-net` intentionally does NOT depend on `craftec-sql` or `craftec-vfs`. The network layer is content-unaware — it transfers opaque bytes identified by CIDs.

### Technology Stack (Exact Versions from Cargo.toml)

| Component | Crate | Version | Notes |
|---|---|---|---|
| Async runtime | `tokio` | `1` | `features = ["full"]` |
| P2P transport | `iroh` | `0.96` | QUIC, NAT traversal, relay. API changed significantly from 0.32+ |
| Wire serialization | `postcard` | `1` | `features = ["alloc"]`; ~60ns encode, ~180ns decode |
| Serde derive | `serde` | `1` | `features = ["derive"]` |
| JSON (config) | `serde_json` | `1` | Config files only; wire uses postcard |
| Hash | `blake3` | `1` | Raw 32-byte output; no multihash wrapper |
| Database | `libsql` | `0.9` | SQLite-compatible; needed for craftec-sql |
| WASM runtime | `wasmtime` | `29` | WASI 0.2; slow to compile (see Part 13) |
| Ed25519 | `ed25519-dalek` | `2` | `features = ["serde", "rand_core"]`; v2 API uses references |
| PRNG | `rand` | `0.8` | Note: `rand::Rng::gen()` conflicts with Rust 2024 `gen` keyword |
| Logging | `tracing` | `0.1` | + `tracing-subscriber 0.3` |
| Error handling | `thiserror` | `2` | v2 has minor API changes from v1 |
| Error boxing | `anyhow` | `1` | binary crate only |
| Bytes | `bytes` | `1` | Zero-copy buffer sharing |
| Hex | `hex` | `0.4` | CID display |
| Concurrent maps | `dashmap` | `6` | Lock-free hash maps |
| Sync primitives | `parking_lot` | `0.12` | Faster than std |
| LRU cache | `lru` | `0.12` | Page cache, object cache |
| Bloom filter | `bloomfilter` | `1` | CID negative lookup |
| Rust edition | — | `2024` | `rust-version = "1.85"` |

---

## PART 1: ENVIRONMENT SETUP

### Rust Toolchain Requirements

```bash
# Install rustup if not present
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Install the exact MSRV
rustup install 1.85
rustup default 1.85

# Verify
rustc --version  # should be ≥ 1.85
cargo --version
```

Rust 2024 edition requires `rust-version = "1.85"` minimum. The workspace already sets this in `[workspace.package]`.

**Critical:** `gen` is a reserved keyword in Rust 2024. Any call to `rand::Rng::gen()` must use `r#gen()` or the `gen_range()`/`random()` alternatives. The existing codebase uses `rand` 0.8 which still works but watch for this in new code.

### System Dependencies

**Ubuntu/Debian:**
```bash
sudo apt-get update
sudo apt-get install -y \
    build-essential \
    pkg-config \
    libssl-dev \
    libclang-dev \      # required by wasmtime/cranelift LLVM bindings
    cmake \             # required by some wasmtime deps
    protobuf-compiler   # required by some iroh deps
```

**macOS:**
```bash
xcode-select --install
brew install cmake protobuf llvm
export LLVM_CONFIG=$(brew --prefix llvm)/bin/llvm-config
```

**wasmtime/cranelift build time warning:** First compile of wasmtime 29 with cranelift takes 10–45 minutes depending on hardware. Always use `--release` for any benchmark. During development, feature-gate wasmtime behind a feature flag to speed up `cargo check` cycles (see Part 13).

**iroh 0.96 OS requirements:** iroh uses QUIC (UDP). Ensure UDP ports are not blocked. For local dev, `0.0.0.0:0` (random port) works fine.

### Cargo Workspace Structure

```
craftec/
├── Cargo.toml              # workspace root (defines members + [workspace.dependencies])
├── Cargo.lock              # COMMIT THIS — reproducible builds matter
├── crates/
│   ├── craftec-types/
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs      # pub mod declarations + re-exports
│   │       ├── cid.rs
│   │       ├── config.rs
│   │       ├── error.rs
│   │       ├── event.rs
│   │       ├── identity.rs
│   │       ├── piece.rs
│   │       └── wire.rs
│   ├── craftec-crypto/
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── hash.rs
│   │       ├── hommac.rs   # PARTIALLY STUBBED — combine_tags is a placeholder
│   │       └── sign.rs
│   ├── craftec-rlnc/
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── gf256.rs    # compile-time lookup tables
│   │       ├── encoder.rs
│   │       ├── decoder.rs
│   │       ├── recoder.rs  # algorithm correct; HomMAC combination is placeholder
│   │       ├── engine.rs   # async concurrency wrapper with Semaphore(8)
│   │       └── error.rs
│   ├── craftec-obj/
│   ├── craftec-vfs/
│   ├── craftec-sql/        # NEEDS libsql integration
│   ├── craftec-net/        # NEEDS SWIM dispatch + handler routing
│   ├── craftec-health/     # NEEDS real piece fetch + scanner wiring
│   ├── craftec-com/        # NEEDS host function implementations + scheduler execution
│   ├── craftec-node/       # NEEDS message routing + event bus routing
│   └── craftec-tests/
│       └── tests/
│           └── multi_node.rs   # 10 integration tests (in-process only)
```

### How to Build and Test at Each Phase

```bash
# Check everything compiles (fast — no codegen)
cargo check --workspace

# Run all unit tests (fast — no real networking)
cargo test --workspace

# Run specific crate tests
cargo test -p craftec-rlnc
cargo test -p craftec-obj
cargo test -p craftec-sql

# Run integration tests
cargo test -p craftec-tests

# Build release binary (slow first time due to wasmtime/cranelift)
cargo build --release -p craftec-node

# Run the node
./target/release/craftec
```

**Development workflow:** Run `cargo check --workspace` after every change. Only run full `cargo test --workspace` before committing — wasmtime tests take significant time.

### CI/CD Setup

Minimal GitHub Actions workflow:

```yaml
# .github/workflows/ci.yml
name: CI
on: [push, pull_request]
jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@1.85
      - uses: Swatinem/rust-cache@v2   # cache target/ between runs
      - name: Install system deps
        run: sudo apt-get install -y libclang-dev cmake protobuf-compiler
      - name: Check
        run: cargo check --workspace
      - name: Test
        run: cargo test --workspace
        env:
          RUST_LOG: warn
```

Cache the `target/` directory — first build of wasmtime is very slow.

---

## PART 2: PHASE 1 — FOUNDATION (craftec-types, craftec-crypto)

### craftec-types

**Purpose:** Shared type definitions used by every other crate. Contains zero business logic — only types, serialization, and basic operations. All other crates depend on this. Changing a type here requires recompilation of all dependents.

**Status: DONE**

#### Complete Cargo.toml

```toml
[package]
name = "craftec-types"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
serde = { workspace = true }
serde_json = { workspace = true }
postcard = { workspace = true }
blake3 = { workspace = true }
bytes = { workspace = true }
hex = { workspace = true }
thiserror = { workspace = true }
ed25519-dalek = { workspace = true }
rand = { workspace = true }
tracing = { workspace = true }

[dev-dependencies]
tempfile = { workspace = true }
```

#### Public Types

**`Cid` (cid.rs)** — Content identifier; raw 32-byte BLAKE3 hash. No multihash wrapper per v3.3 design decision.

```rust
pub const CID_SIZE: usize = 32;

#[derive(Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Cid([u8; CID_SIZE]);

impl Cid {
    pub fn from_bytes(bytes: [u8; CID_SIZE]) -> Self
    pub fn from_data(data: &[u8]) -> Self      // BLAKE3(data)
    pub fn verify(&self, data: &[u8]) -> bool  // re-hash and compare
    pub fn as_bytes(&self) -> &[u8; CID_SIZE]
}
// Display: lowercase hex (64 chars)
// FromStr: parse 64-char hex, returns CraftecError::IdentityError on wrong length
// Serialize: hex string (human-readable), raw bytes (binary)
```

**`NodeConfig` (config.rs)** — Node configuration loaded from JSON.

```rust
pub struct NodeConfig {
    pub data_dir: PathBuf,              // default: "data"
    pub listen_port: u16,               // default: 4433
    pub bootstrap_peers: Vec<String>,   // default: []
    pub max_connections: usize,         // default: 256
    pub max_disk_usage_bytes: u64,      // default: 10 GiB
    pub health_scan_interval_secs: u64, // default: 3600
    pub rlnc_k: u32,                    // default: 32; NOT hot-reloadable
    pub page_size: usize,               // default: 16_384
    pub log_level: String,              // default: "info"
}
```

Configuration is loaded from a JSON file. The spec (§13) uses TOML but the implementation uses JSON — both are acceptable. The spec parameters (`max_connections=200`, `peer_timeout_secs=120`, etc.) are not yet all represented in the current `NodeConfig` struct and should be added.

**`CraftecError` (error.rs)** — Top-level error type shared across crates.

```rust
#[derive(Debug, thiserror::Error)]
pub enum CraftecError {
    #[error("Storage error: {0}")]
    StorageError(String),
    #[error("Network error: {0}")]
    NetworkError(String),
    #[error("Coding error: {0}")]
    CodingError(String),
    #[error("Identity error: {0}")]
    IdentityError(String),
    #[error("Database error: {0}")]
    DatabaseError(String),
    #[error("WASM error: {0}")]
    WasmError(String),
    #[error(transparent)]
    IoError(#[from] std::io::Error),
    #[error("Serialization error: {0}")]
    SerializationError(String),
}
pub type Result<T> = std::result::Result<T, CraftecError>;
```

**`Event` (event.rs)** — Internal pub/sub events routed via the event bus (§52).

```rust
#[derive(Debug, Clone)]
pub enum Event {
    CidWritten { cid: Cid },
    PageCommitted { db_id: Cid, page_num: u32, root_cid: Cid },
    PeerConnected { node_id: NodeId },
    PeerDisconnected { node_id: NodeId },
    RepairNeeded { cid: Cid, available: u32, target: u32 },
    DiskWatermarkHit { usage_percent: f64 },
    ShutdownSignal,
}

// Channel capacities (from spec §52):
pub const CID_WRITTEN_CAP: usize = 256;
pub const PAGE_COMMITTED_CAP: usize = 256;
pub const PEER_EVENT_CAP: usize = 256;
pub const REPAIR_NEEDED_CAP: usize = 256;
pub const DISK_WATERMARK_CAP: usize = 64;
pub const SHUTDOWN_CAP: usize = 8;
```

**`NodeId` and `NodeKeypair` (identity.rs)** — Ed25519 identity. NodeId IS the 32-byte public key — no hashing step.

```rust
#[derive(Copy, Clone, PartialEq, Eq, Hash)]
pub struct NodeId([u8; 32]);

pub struct Signature(ed25519_dalek::Signature);

pub struct NodeKeypair {
    signing_key: SigningKey,  // private field
}

impl NodeKeypair {
    pub fn generate() -> Self
    pub fn from_signing_key(signing_key: SigningKey) -> Self
    pub fn node_id(&self) -> NodeId
    pub fn public_key(&self) -> NodeId        // alias for node_id()
    pub fn sign(&self, msg: &[u8]) -> Signature
    pub fn to_secret_bytes(&self) -> [u8; 32]
    pub fn from_secret_bytes(bytes: &[u8; 32]) -> Self
}

pub fn verify(msg: &[u8], sig: &Signature, node_id: &NodeId) -> bool
```

**`PieceId`, `CodedPiece`, `PieceIndex` (piece.rs)** — RLNC piece types.

```rust
pub const K_DEFAULT: u32 = 32;
pub const PAGE_SIZE: usize = 16_384;   // 16 KB SQLite pages
pub const GF_ORDER: u32 = 256;

pub type HomMAC = [u8; 32];

#[derive(Copy, Clone, PartialEq, Eq, Hash)]
pub struct PieceId([u8; 32]);

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CodedPiece {
    pub piece_id: PieceId,        // BLAKE3(coding_vector || data) — computed on construction
    pub cid: Cid,                 // CID of the original content
    pub coding_vector: Vec<u8>,   // k bytes — one per source piece
    pub data: Vec<u8>,            // coded_data = Σ cv[j] * piece[j] over GF(2^8)
    pub hommac_tag: [u8; 32],
}

impl CodedPiece {
    // Constructs piece and auto-computes piece_id = BLAKE3(coding_vector || data)
    pub fn new(cid: Cid, coding_vector: Vec<u8>, data: Vec<u8>, hommac_tag: [u8; 32]) -> Self
    pub fn verify_piece_id(&self) -> bool
    pub fn verify_mac(&self) -> bool  // BLAKE3(cid_bytes || coding_vector || data) == hommac_tag
}

pub fn redundancy(k: u32) -> f64 { 2.0 + 16.0 / k as f64 }
pub fn target_n(k: u32) -> u32   { (k as f64 * redundancy(k)).ceil() as u32 }
// For k=32: redundancy=2.5, target_n=80
```

**`WireMessage` (wire.rs)** — All P2P messages serialized with postcard.

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum WireMessage {
    Ping { nonce: u64 },
    Pong { nonce: u64 },
    PieceRequest { cid: Cid, piece_indices: Vec<u32> },
    PieceResponse { pieces: Vec<CodedPiece> },
    ProviderAnnounce { cid: Cid, node_id: NodeId },
    SignedWrite {
        payload: Vec<u8>,       // serialized write instruction
        signature: Signature,
        writer: NodeId,
        cas_version: u64,
    },
    SwimJoin { node_id: NodeId, listen_port: u16 },
    SwimAlive { node_id: NodeId, incarnation: u64 },
    SwimSuspect { node_id: NodeId, incarnation: u64, from: NodeId },
    SwimDead { node_id: NodeId, incarnation: u64, from: NodeId },
    HealthReport { cid: Cid, available_pieces: u32, target_pieces: u32 },
    SwimPing { from: NodeId, piggyback: Vec<WireMessage> },
}

// Wire encoding (no separate framing layer in current impl):
pub fn encode(msg: &WireMessage) -> Result<Vec<u8>>   // postcard::to_allocvec
pub fn decode(data: &[u8]) -> Result<WireMessage>     // postcard::from_bytes
```

**Note on wire format vs spec §23:** The spec defines a 9-byte fixed header `[type_tag: u32 | version: u8 | payload_len: u32]`. The current implementation uses postcard's self-describing format without an explicit header. The SIGNED_WRITE (0x06000001) and WRITE_RESULT (0x06000002) message types from the spec are partially represented via the `SignedWrite` variant. The framing header should be added when implementing the full binary protocol.

#### Tests to Write

All existing tests pass. Add:
- `wire_frame_header_encoding` — verify 9-byte header format per spec §23
- `config_toml_round_trip` — if switching to TOML per spec §13
- `node_id_iroh_compatibility` — verify `NodeId` bytes match `iroh::PublicKey` bytes

#### Integration Points

`craftec-types` exports are re-used everywhere. Any change to `Cid`, `WireMessage`, or `CodedPiece` requires rebuilding all 9 other crates. Keep this crate stable.

---

### craftec-crypto

**Purpose:** Cryptographic primitives: BLAKE3 hashing, HomMAC for piece integrity, Ed25519 KeyStore (load/generate keypair from disk).

**Status: MOSTLY DONE** — `combine_tags()` needs real Catalano-Fiore HomMAC.

#### Complete Cargo.toml

```toml
[package]
name = "craftec-crypto"
version.workspace = true
edition.workspace = true

[dependencies]
craftec-types = { workspace = true }
blake3 = { workspace = true }
ed25519-dalek = { workspace = true }
rand = { workspace = true }
serde = { workspace = true }
thiserror = { workspace = true }
tracing = { workspace = true }
hex = { workspace = true }

[dev-dependencies]
tempfile = { workspace = true }
```

#### Public API

**`hash.rs`:**
```rust
pub fn hash_bytes(data: &[u8]) -> [u8; 32]
    // blake3::hash(data).as_bytes().clone()

pub fn hash_page(page_data: &[u8]) -> Cid
    // Cid::from_data(page_data) — convenience wrapper for SQL VFS

pub fn verify_cid(data: &[u8], expected: &Cid) -> bool
    // expected.verify(data)

pub fn merkle_root(leaves: &[[u8; 32]]) -> [u8; 32]
    // Binary Merkle tree over BLAKE3
    // Empty input → [0u8; 32]
    // Odd layers: duplicate last node (Bitcoin-style)
```

**`hommac.rs` — Current state vs. needed:**

```rust
pub struct HomMacKey([u8; 32]);

impl HomMacKey {
    pub fn generate() -> Self           // OsRng
    pub fn from_bytes(bytes: [u8; 32]) -> Self
    pub fn as_bytes(&self) -> &[u8; 32]
}

// FULLY IMPLEMENTED — BLAKE3 keyed hash
pub fn compute_tag(key: &HomMacKey, coding_vector: &[u8], data: &[u8]) -> [u8; 32]
    // BLAKE3(key || coding_vector || data)

// FULLY IMPLEMENTED
pub fn verify_tag(key: &HomMacKey, coding_vector: &[u8], data: &[u8], tag: &[u8; 32]) -> bool

// STUBBED — uses GF(2^8) XOR accumulation, then re-authenticates with BLAKE3
// Comment in code: "Replace with proper algebraic HomMAC (Catalano-Fiore)"
pub fn combine_tags(key: &HomMacKey, tags: &[[u8; 32]], coefficients: &[u8]) -> [u8; 32]
```

**What `combine_tags` currently does (wrong):**
1. XOR-accumulates tags weighted by coefficients (not linearly homomorphic)
2. Re-authenticates the result with `BLAKE3(key || combined || coefficients)`

**What it should do (Catalano-Fiore over GF(2^8)):**
The HomMAC tag for a recoded piece must satisfy: if `w = Σ αᵢ·vᵢ` (recoded piece), then `tag(w) = Σ αᵢ·tag(vᵢ)` using GF(2^8) scalar multiplication over the 32-byte tag field. The critical property: any downstream relay can verify `tag(w) == combine_tags(tags, coefficients)` **without the key**. The current BLAKE3-based approach requires the key for verification, which defeats the purpose.

**Minimal correct implementation for combine_tags:**
```rust
pub fn combine_tags(key: &HomMacKey, tags: &[[u8; 32]], coefficients: &[u8]) -> [u8; 32] {
    assert_eq!(tags.len(), coefficients.len());
    let mut result = [0u8; 32];
    for (tag, &coeff) in tags.iter().zip(coefficients.iter()) {
        for i in 0..32 {
            // GF(2^8) multiply each tag byte by the coefficient, then XOR into result
            result[i] ^= gf256_mul(coeff, tag[i]);
        }
    }
    result
}
```

This is homomorphic: `verify_combined(result, combined_cv, combined_data)` uses the same BLAKE3-keyed structure. See spec §15 for the full vtag verification protocol.

**Polynomial inconsistency:** `gf256.rs` uses AES polynomial `0x11B`; `hommac.rs` uses `0x11D`. Standardize on `0x11B` (AES standard, used in the encoder/decoder hot path).

**`sign.rs` — KeyStore:**
```rust
pub struct KeyStore {
    keypair: NodeKeypair,
    key_path: PathBuf,
}

impl KeyStore {
    pub fn new(data_dir: &Path) -> Result<Self>
        // Loads {data_dir}/node.key (32 raw bytes) if exists.
        // If not: generate NodeKeypair, write secret bytes, set permissions to 0600.
        // Returns IdentityError if file has wrong length.

    pub fn sign(&self, msg: &[u8]) -> Signature
    pub fn verify(&self, msg: &[u8], sig: &Signature, pubkey: &NodeId) -> bool
    pub fn node_id(&self) -> NodeId
    pub fn key_path(&self) -> &Path
}
```

**ed25519-dalek v2 API note:** `SigningKey::from_bytes()` takes `&[u8; 32]` (a reference), not a value. If you see `from_bytes takes reference not value` errors, this is why. Use:
```rust
let signing_key = SigningKey::from_bytes(&secret_bytes);
// NOT: SigningKey::from_bytes(secret_bytes)
```

#### Tests to Write

Existing tests are comprehensive. Add:
- `combine_tags_is_homomorphic` — verify `combine_tags([t1, t2], [α1, α2])` matches `compute_tag` on the linear combination
- `gf_polynomial_consistency` — verify `gf256_mul` in hommac.rs and gf256.rs agree on all inputs

---

## PART 3: PHASE 2 — ERASURE CODING (craftec-rlnc)

### craftec-rlnc

**Purpose:** Random Linear Network Coding over GF(2^8). Provides encode, decode (Gaussian elimination), recode (without decoding), and a concurrency-limited async engine. This is a **kernel-level** component — must be available before any WASM loads. SIMD vectorization should be applied to `gf_vec_mul_add`.

**Status: MOSTLY DONE** — All algorithms correct. HomMAC combination in recoder is a placeholder.

#### Complete Cargo.toml

```toml
[package]
name = "craftec-rlnc"
version.workspace = true
edition.workspace = true

[dependencies]
craftec-types = { workspace = true }
craftec-crypto = { workspace = true }
blake3 = { workspace = true }
bytes = { workspace = true }
tokio = { workspace = true }
rand = { workspace = true }
tracing = { workspace = true }
thiserror = { workspace = true }

[dev-dependencies]
tempfile = { workspace = true }
tokio = { workspace = true }
```

#### GF(2^8) Arithmetic (gf256.rs)

The field uses the AES irreducible polynomial `x^8 + x^4 + x^3 + x + 1 = 0x11B`. Lookup tables are computed at compile time using `const fn`.

```rust
// Compiled into the binary — no runtime initialization
pub static EXP_TABLE: [u8; 512] = build_exp_table();
// EXP_TABLE[i] = g^i where g = 0x03 (primitive element)
// Duplicated: EXP_TABLE[i] = EXP_TABLE[i+255] for all i

pub static LOG_TABLE: [u8; 256] = build_log_table(&EXP_TABLE);
// LOG_TABLE[a] = log_g(a); LOG_TABLE[0] = 0 (convention)

// Field operations:
pub fn gf_add(a: u8, b: u8) -> u8 { a ^ b }
pub fn gf_mul(a: u8, b: u8) -> u8 {
    if a == 0 || b == 0 { return 0; }
    EXP_TABLE[(LOG_TABLE[a as usize] as usize + LOG_TABLE[b as usize] as usize) % 255]
}
pub fn gf_div(a: u8, b: u8) -> u8 {
    debug_assert!(b != 0);
    if a == 0 { return 0; }
    EXP_TABLE[(LOG_TABLE[a as usize] as usize + 255 - LOG_TABLE[b as usize] as usize) % 255]
}
pub fn gf_inv(a: u8) -> u8 {
    debug_assert!(a != 0);
    EXP_TABLE[255 - LOG_TABLE[a as usize] as usize]
}

// Hot loop — SAXPY over GF(2^8). SIMD optimization target.
// dst[i] ^= coeff * src[i]
pub fn gf_vec_mul_add(dst: &mut [u8], src: &[u8], coeff: u8) {
    debug_assert_eq!(dst.len(), src.len());
    if coeff == 0 { return; }
    if coeff == 1 {
        for (d, &s) in dst.iter_mut().zip(src.iter()) { *d ^= s; }
        return;
    }
    let log_c = LOG_TABLE[coeff as usize] as usize;
    for (d, &s) in dst.iter_mut().zip(src.iter()) {
        if s != 0 {
            *d ^= EXP_TABLE[log_c + LOG_TABLE[s as usize] as usize];
        }
    }
}
```

**SIMD opportunity:** The `gf_vec_mul_add` function is the hottest loop in the system. For production throughput (~75 Gbps per spec §28), use SIMD intrinsics or the `galois-field` crate's SIMD implementation. The spec notes GF(2^8) enables "SIMD vectorization" — this is the function to optimize.

#### Encoder (encoder.rs)

```rust
pub struct RlncEncoder {
    k: u32,
    piece_size: usize,
    original_pieces: Vec<Vec<u8>>,
    cid: Cid,
}

impl RlncEncoder {
    /// Split `data` into k pieces. Last piece zero-padded to `piece_size`.
    /// CID = BLAKE3(data) (the full original, before splitting).
    /// Returns CodingError if k == 0.
    pub fn new(data: &[u8], k: u32) -> Result<Self>

    pub fn k(&self) -> u32
    pub fn piece_size(&self) -> usize
    pub fn cid(&self) -> &Cid
    pub fn target_pieces(&self) -> u32   // ceil(k * redundancy(k))

    /// Generate one coded piece with fresh random GF(2^8) coefficients.
    /// Guarantees all coefficients are non-zero (dense coding vectors).
    /// piece_id = BLAKE3(coding_vector || coded_data)
    /// hommac_tag = BLAKE3(key || coding_vector || coded_data) where key = *cid.as_bytes()
    pub fn encode_piece(&self) -> CodedPiece

    /// Convenience: encode_piece() called n times.
    pub fn encode_n(&self, n: usize) -> Vec<CodedPiece>
}
```

**Piece format (spec §15):**
- `coding_vector`: k bytes, each drawn uniformly from {1..255} (non-zero ensures linear independence over GF(2^8))
- `data`: piece_size bytes = Σ cv[j] * original_pieces[j] (via `gf_vec_mul_add`)
- `piece_id`: `BLAKE3(coding_vector || data)` — authenticates the specific combination
- `hommac_tag`: `BLAKE3(cid_bytes || coding_vector || data)` — allows downstream HomMAC verification

**Note on piece_size:** `piece_size = ceil(data.len() / k)`. For 8 MiB = 8,388,608 bytes with k=32: piece_size = 262,144 bytes = 256 KiB. For SQLite pages (16 KB each), k defaults to 8 (not 32), giving piece_size = 16,384 bytes exactly (no padding needed for aligned pages).

#### Decoder (decoder.rs)

```rust
pub struct RlncDecoder {
    k: u32,
    piece_size: usize,
    // Augmented matrix: k rows × (k + piece_size) columns
    // Left k columns: coding vectors (reduced to identity during elimination)
    // Right piece_size columns: coded data (produces original pieces after back-substitution)
    matrix: Vec<Vec<u8>>,
    pivot_col: Vec<Option<usize>>,  // which column each row pivots on
    rank: u32,
    decoded: bool,
}

impl RlncDecoder {
    pub fn new(k: u32, piece_size: usize) -> Self
    pub fn k(&self) -> u32
    pub fn rank(&self) -> u32
    pub fn is_decodable(&self) -> bool   // rank == k
    pub fn progress(&self) -> f64        // rank / k

    /// Add a piece to the decoding matrix.
    /// Performs forward elimination (partial Gaussian elimination).
    /// Returns Ok(true) if piece was linearly independent (increased rank).
    /// Returns Ok(false) if piece was dependent (no rank increase, can discard).
    /// Returns Err on malformed piece (wrong cv length or data size).
    pub fn add_piece(&mut self, piece: &CodedPiece) -> Result<bool>

    /// Perform back-substitution. Requires rank == k.
    /// Returns concatenated original pieces (k * piece_size bytes).
    /// Caller must know original data length to trim zero-padding from last piece.
    pub fn decode(&mut self) -> Result<Vec<u8>>

    pub fn is_decoded(&self) -> bool
}
```

**Gaussian elimination implementation:**
```
For each new piece (cv, data):
  1. Find the first non-zero position in cv that has no existing pivot
  2. If none exists: piece is dependent → return false
  3. Normalize row: multiply all entries by gf_inv(cv[pivot_pos])
  4. Eliminate this position from all existing rows using gf_vec_mul_add
  5. Record pivot position, increment rank
Back-substitution for decode():
  1. For each row from bottom: already in row-echelon form
  2. Use pivot rows to zero out entries above each pivot
  3. Right-hand side columns now contain original pieces
```

**CRITICAL — Decode is client-side only (spec §28):** Storage nodes never call `decode()`. They only hold coded pieces and serve them on request. The `Semaphore(8)` in `RlncEngine` is sized for client-side decode concurrency. Memory budget: 8 × 32 × 256 KiB = 64 MB.

#### Recoder (recoder.rs)

```rust
pub struct RlncRecoder;  // stateless

impl RlncRecoder {
    /// Generate a new coded piece from ≥ 2 existing coded pieces.
    /// All input pieces must have the same CID (same content).
    /// NO DECODE REQUIRED — this is the key RLNC advantage for repair.
    ///
    /// Algorithm:
    ///   1. Draw random non-zero scalars α₁..αₙ for each input piece
    ///   2. new_cv[j] = Σ αᵢ * pieces[i].coding_vector[j]  (via gf_vec_mul_add)
    ///   3. new_data = Σ αᵢ * pieces[i].data              (via gf_vec_mul_add)
    ///   4. new piece_id = BLAKE3(new_cv || new_data)
    ///   5. new hommac_tag = combine_tags(tags, scalars)   [currently placeholder]
    ///
    /// Returns:
    ///   InsufficientRecodeInputs if pieces.len() < 2
    ///   MismatchedCids if pieces have different CIDs
    pub fn recode(pieces: &[CodedPiece]) -> Result<CodedPiece>
}
```

**Repair bandwidth advantage:** A repair node recodes from its own ≥2 locally-held pieces — no network fetch needed for recode input. This is far cheaper than Reed-Solomon repair (which requires fetching K pieces to reconstruct, then re-encode). The only network cost per repair is distributing 1 new coded piece to a non-holder.

#### Generation Engine (engine.rs)

```rust
pub struct RlncEngine {
    semaphore: Arc<Semaphore>,  // MAX_CONCURRENCY = 8
    metrics: Arc<RlncMetrics>,
}

impl RlncEngine {
    pub const MAX_CONCURRENCY: usize = 8;
    pub fn new() -> Self

    /// Encode data into target_n coded pieces.
    /// Acquires semaphore before encoding; releases after.
    pub async fn encode(&self, data: &[u8], k: u32) -> Result<Vec<CodedPiece>>

    /// Decode k or more pieces back to original data.
    /// Skips malformed pieces (logs warning) instead of failing.
    /// Returns InsufficientPieces if fewer than k independent pieces after filtering.
    pub async fn decode(&self, k: u32, piece_size: usize, pieces: &[CodedPiece]) -> Result<Vec<u8>>

    /// Recode existing pieces into a fresh coded piece.
    pub async fn recode(&self, pieces: &[CodedPiece]) -> Result<CodedPiece>

    pub fn metrics(&self) -> &RlncMetrics
}
```

**Concurrency rationale:** `Semaphore(8)` bounds concurrent decodes to 8. Each decode allocates ~`k * piece_size * 2` bytes for the augmented matrix. At k=32, piece_size=256KiB: 32 × 256KiB × 2 ≈ 16 MB per decode. 8 concurrent = 128 MB peak. Within the 256 MB RLNC buffer budget (spec §44).

#### Piece Format Summary

Per spec §15, every piece is self-describing:
```
PieceHeader {
    content_id:    [u8; 32],   // BLAKE3 of original content = CID
    segment_idx:   u32,        // which segment of a multi-segment object
    segment_count: u32,        // total segments
    k:             u32,        // source piece count
    vtags_cid:     Option<[u8; 32]>,  // CID of vtag vector for HomMAC verification
    coefficients:  [u8; k],    // RLNC coding vector
}
// piece_id = BLAKE3(coefficients)  [used in current impl as BLAKE3(cv || data)]
```

The current `CodedPiece` struct omits `segment_idx` and `segment_count` — these are needed for multi-segment large files (>8 MiB). For MVP, single-segment files are sufficient.

#### HomMAC: Current XOR Stub vs. Catalano-Fiore

**Current behavior (stub):**
```
combine_tags(key, [t1,t2], [α1,α2]):
  combined = gf_mul(α1, t1[0]) ^ gf_mul(α2, t2[0])  ‖ ...  (XOR-based)
  result = BLAKE3(key ‖ combined ‖ [α1,α2])
```
This result cannot be independently verified by a relay without the key. The key is implicitly `HomMacKey::from_bytes(*cid.as_bytes())` in the encoder, so every node that holds the CID can derive the key — but the combination is not algebraically correct.

**Correct behavior (Catalano-Fiore):**
```
combine_tags(key, [t1,t2], [α1,α2]):
  result[i] = gf_mul(α1, t1[i]) ^ gf_mul(α2, t2[i])  for each byte i
```
This is homomorphic: `verify_tag(key, combined_cv, combined_data, result)` works correctly for recoded pieces. See the fix in the crypto section above.

**Security impact of current stub:** The stub is safe for correctness (all nodes use the same derivation), but a malicious node could forge a `combine_tags` result that passes BLAKE3 verification with a brute-forced key. Risk level for MVP: LOW (requires key knowledge to forge, and keys are CID-derived). Fix before production.

#### Performance Considerations at Scale

| Operation | Throughput | Bottleneck |
|---|---|---|
| Encode (k=32, 256 KiB pieces) | ~75 Gbps single core | `gf_vec_mul_add` SAXPY |
| Decode (k=32, 256 KiB pieces) | ~48 Gbps single core | Gaussian elimination |
| Recode | ~same as encode | `gf_vec_mul_add` |
| BLAKE3 per 16 KB page | ~2 µs | Memory bandwidth |

For a million-node network, the bottleneck is network bandwidth, not computation. Each node handles at most ~200 concurrent connections, not 1M. The Semaphore(8) is the right choice.

#### Tests to Write

All existing tests pass. Add:
- `encoder_segment_metadata` — verify `segment_idx`/`segment_count` fields when added
- `decode_with_recoded_pieces` — decode using only recoded pieces (no originals)
- `repair_bandwidth_invariant` — recode from 2 pieces, verify result is decodable with k-2 originals
- `simd_gf_vec_mul_add_matches_scalar` — parity check if SIMD is added

---

## PART 4: PHASE 3 — CONTENT-ADDRESSED STORAGE (craftec-obj)

### craftec-obj

**Purpose:** Local content-addressed object store. `put(data) → CID`, `get(CID) → data`. Three-layer read path: LRU cache → bloom filter → disk. Write path: temp file → fsync → atomic rename. BLAKE3 integrity on every disk read.

**Status: DONE** — Fully implemented with 26+ tests.

#### Store Architecture

```
data/obj/
├── 00/                 # shard directory (first byte of CID in hex)
│   ├── 0000abc...def  # filename = full 64-char hex CID
│   └── 0042bcd...123
├── 01/
│   └── ...
└── ff/
```

256 shard directories, named `00` through `ff` (first byte of CID hex). CID is the filename — no separate database needed. This is the key design decision: the filesystem IS the index (spec §18 "CID = filename").

```rust
pub fn shard_path(base: &Path, cid: &Cid) -> PathBuf {
    let hex = cid.to_string();  // 64-char lowercase hex
    base.join(&hex[..2]).join(&hex)
}
```

#### Bloom Filter (bloom.rs)

Fast negative lookup — avoids disk stat() for absent CIDs.

```rust
pub struct CidBloomFilter {
    inner: Bloom<[u8; 32]>,
    count: usize,
}

impl CidBloomFilter {
    pub fn new(expected_items: usize, fp_rate: f64) -> Self
        // defaults: 1_000_000 items, 1% FP rate
    pub fn insert(&mut self, cid: &Cid)
    pub fn probably_contains(&self, cid: &Cid) -> bool
    pub fn rebuild(base_dir: &Path) -> Result<Self>
        // Walk all 256 shard dirs, insert all CIDs found on disk.
        // Called on startup to rebuild bloom from existing files.
}
```

**FP rate impact:** 1% FP rate means 1 in 100 absent-CID lookups will proceed to disk. At 1M stored CIDs, the bloom filter uses ~1.2 MB memory. Acceptable tradeoff.

**Important:** After `delete()`, the bloom filter is NOT updated (still returns `probably_contains = true`). Harmless: the subsequent disk stat() will find the file absent. Bloom filters are append-only in this design.

#### LRU Page Cache (cache.rs)

```rust
pub struct ObjectCache {
    inner: RwLock<LruCache<Cid, Bytes>>,
    capacity: usize,
}

impl ObjectCache {
    pub fn new(capacity: usize) -> Self  // panics if capacity == 0
    pub fn get(&self, cid: &Cid) -> Option<Bytes>   // write lock (updates MRU position)
    pub fn peek(&self, cid: &Cid) -> Option<Bytes>  // read lock (no recency update)
    pub fn put(&self, cid: Cid, data: Bytes)
    pub fn remove(&self, cid: &Cid) -> Option<Bytes>
    pub fn len(&self) -> usize
    pub fn contains(&self, cid: &Cid) -> bool
    pub fn clear(&self)
}
```

Cache capacity is measured in item count, not bytes. Default: 1024 items in `craftec-node`. At 16 KB pages, 1024 items = 16 MB. For production, increase to `16_384` (= 256 MB budget from spec §27).

#### Write Path

```rust
pub async fn put(&self, data: &[u8]) -> Result<Cid> {
    let cid = Cid::from_data(data);  // BLAKE3(data)

    // 1. Deduplication check (bloom + stat) — avoid re-writing existing content
    if self.inner.bloom.read().probably_contains(&cid) {
        let path = shard_path(&self.inner.base_dir, &cid);
        if path.exists() {
            self.inner.cache.put(cid, Bytes::copy_from_slice(data));
            return Ok(cid);
        }
    }

    // 2. Write to temp file in same directory (same filesystem = atomic rename)
    let shard = shard_path(&self.inner.base_dir, &cid);
    let tmp = shard.with_extension("tmp");
    let mut file = tokio::fs::File::create(&tmp).await?;
    file.write_all(data).await?;
    file.sync_all().await?;   // fsync before rename — durability
    tokio::fs::rename(&tmp, &shard).await?;

    // 3. Update bloom + cache
    self.inner.bloom.write().insert(&cid);
    self.inner.cache.put(cid, Bytes::copy_from_slice(data));
    self.inner.metrics.puts.fetch_add(1, Ordering::Relaxed);
    Ok(cid)
}
```

**ENOSPC handling:** `std::io::Error` with `kind() == ErrorKind::StorageFull` or errno 28 is mapped to `ObjError::StoreFull`. The disk space watermarks (§32) should check before writing.

#### Read Path

```rust
pub async fn get(&self, cid: &Cid) -> Result<Option<Bytes>> {
    // Layer 1: LRU cache (~50ns hit latency)
    if let Some(data) = self.inner.cache.get(cid) {
        self.inner.metrics.cache_hits.fetch_add(1, Ordering::Relaxed);
        return Ok(Some(data));
    }
    self.inner.metrics.cache_misses.fetch_add(1, Ordering::Relaxed);

    // Layer 2: Bloom filter — fast negative (skips disk for absent CIDs)
    if !self.inner.bloom.read().probably_contains(cid) {
        return Ok(None);
    }

    // Layer 3: Disk read with BLAKE3 integrity verification
    let path = shard_path(&self.inner.base_dir, cid);
    match tokio::fs::read(&path).await {
        Ok(data) => {
            // Integrity check — verify BLAKE3(data) == cid
            if !cid.verify(&data) {
                // Corruption detected. Delete the corrupt file.
                let _ = tokio::fs::remove_file(&path).await;
                self.inner.metrics.integrity_violations.fetch_add(1, Ordering::Relaxed);
                return Err(ObjError::IntegrityViolation {
                    cid: cid.to_string(),
                    msg: "BLAKE3 hash mismatch".to_string(),
                });
            }
            self.inner.cache.put(*cid, Bytes::copy_from_slice(&data));
            Ok(Some(Bytes::from(data)))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(ObjError::IoError(e)),
    }
}
```

#### Eviction Policy

The current store has `delete(cid)` but no automatic eviction. Eviction is driven by:
1. Disk watermark events (§32): `DiskWatermarkHit` event triggers LRU eviction of non-authoritative pieces
2. Local eviction agent (WASM, §31): runs daily mark-and-sweep, decides priority ordering
3. Manual: `delete(cid)` can be called directly

**Node's own data is never evicted.** Priority ordering per spec §31: funded pieces > critical CIDs > recently served > own content. The WASM eviction agent defines this policy — the kernel-level `ContentAddressedStore` just provides `delete()`.

#### All Tests Required

Existing 26 tests cover the core paths. Additional tests to write:
- `concurrent_puts_same_cid` — verify deduplication under concurrent writes
- `put_after_delete_rewrites` — deleted then re-put should succeed
- `bloom_rebuild_after_restart` — `CidBloomFilter::rebuild()` finds all existing files
- `integrity_violation_triggers_delete` — corrupt file is removed and `Ok(None)` returned on next `get()`
- `disk_full_returns_store_full` — mock ENOSPC

---

## PART 5: PHASE 4 — CID-VFS (craftec-vfs)

### craftec-vfs

**Purpose:** Bridges `craftec-obj` (content-addressed storage) and `craftec-sql` (SQLite). Maps SQLite page I/O (xRead/xWrite/xSync) to CID-based operations. Each SQLite page = one CID in CraftOBJ. Commit point = atomic swap of root CID.

**Status: DONE** — Fully implemented. Needs to be wired into real SQLite as a VFS plugin.

#### SQLite Pages as Content-Addressed Objects

Every 16 KB SQLite page is stored as a CID in `craftec-obj`:
- **Write:** `xWrite(page_num, data)` → buffer in `dirty_pages` map (no I/O yet)
- **Commit:** `xSync()` → hash all dirty pages → write to CraftOBJ → update page index → return new root CID
- **Read:** `xRead(page_num)` → look up CID in page index → `store.get(cid)` → verify BLAKE3 → return data

```
SQLite                   CID-VFS                    CraftOBJ
  │                         │                           │
  │─ xWrite(3, data) ──────►│                           │
  │                    dirty_pages[3] = data            │
  │                         │                           │
  │─ xSync() ──────────────►│                           │
  │                    for (pn, data) in dirty_pages:   │
  │                    cid = BLAKE3(data) ─────────────►│
  │                    page_index[pn] = cid             │ store.put(data)
  │                    index_bytes = serialize(index)   │
  │                    index_cid = BLAKE3(index_bytes)  │
  │                    root_cid = index_cid             │
  │◄─ SQLITE_OK ───────────-│                           │
```

#### Page Index (page_index.rs)

```rust
pub struct PageIndex {
    entries: RwLock<HashMap<u32, Cid>>,  // page_num → CID
    root_cid: RwLock<Option<Cid>>,
}

impl PageIndex {
    pub fn get(&self, page_num: u32) -> Option<Cid>
    pub fn set(&self, page_num: u32, cid: Cid)
    pub fn remove(&self, page_num: u32)
    pub fn root(&self) -> Option<Cid>

    /// Binary serialization: [u32 LE count][u32 LE page_num][32 bytes CID]...
    /// Deterministic: sorted by page_num ascending.
    /// The index itself is stored as a CID in CraftOBJ — the root CID points to the index.
    pub fn serialize(&self) -> Vec<u8>
    pub fn deserialize(data: &[u8]) -> Result<Self>
}
```

**Root CID semantics:** The root CID is `BLAKE3(serialize(page_index))`. Because the page index maps every page number to a CID, and each CID is content-addressed, the root CID cryptographically commits the entire database state. This is snapshot isolation for free: pin the root CID, and all referenced pages are immutable.

#### Snapshot (snapshot.rs)

```rust
pub struct Snapshot {
    pub root_cid: Cid,
    entries: Arc<HashMap<u32, Cid>>,   // FROZEN at construction — won't see new writes
    pub created_at: Instant,
    page_index: Arc<PageIndex>,        // live reference (diagnostics only)
}

impl Snapshot {
    /// Create snapshot from current page_index state.
    /// entries is a point-in-time copy — mutations to page_index after this don't affect the snapshot.
    pub fn new(root_cid: Cid, page_index: Arc<PageIndex>) -> Self

    /// Resolve a page CID using the FROZEN entries (snapshot isolation).
    /// Returns None if page was not present at snapshot time.
    pub fn resolve_page(&self, page_num: u32) -> Option<Cid>
}
```

#### CidVfs (vfs.rs)

```rust
pub const DEFAULT_PAGE_SIZE: usize = 16_384;  // 16 KB per spec §33

pub struct CidVfs {
    store: Arc<ContentAddressedStore>,
    page_index: Arc<PageIndex>,
    page_cache: Arc<PageCache>,
    dirty_pages: Mutex<HashMap<u32, Vec<u8>>>,
    current_root: RwLock<Option<Cid>>,
    page_size: usize,
}

impl CidVfs {
    pub fn new(store: Arc<ContentAddressedStore>, page_size: usize) -> Result<Self>
        // Validates page_size is power-of-two in [512, 65536]

    /// Read a page: page_cache → page_index → store.get → verify
    pub async fn read_page(&self, page_num: u32) -> Result<Vec<u8>>
        // Returns PageNotFound if no CID for this page_num
        // Returns IntegrityCheckFailed if BLAKE3 mismatch

    /// Buffer page data (no I/O until commit).
    pub fn write_page(&self, page_num: u32, data: &[u8]) -> Result<()>
        // Validates data.len() == page_size

    /// Commit: flush dirty pages, update index, return root CID.
    pub async fn commit(&self) -> Result<Cid>
        // 1. For each dirty page: store.put(data) → get CID → page_index.set(page_num, cid)
        // 2. Serialize page_index
        // 3. index_cid = store.put(index_bytes)
        // 4. Atomically: current_root = index_cid; page_index.set_root(index_cid)
        // 5. Clear dirty_pages
        // Returns the new root CID

    /// Pin current database state as a snapshot for reads.
    pub fn snapshot(&self) -> Result<Snapshot>
        // Returns NoRootCid if no commits yet

    pub fn current_root(&self) -> Option<Cid>
    pub fn page_size(&self) -> usize
}
```

#### Page Cache (page_cache.rs)

```rust
pub struct PageCache {
    cache: Mutex<LruCache<(Cid, u32), Vec<u8>>>,  // key: (root_cid, page_num)
    hits: AtomicU64,
    misses: AtomicU64,
}
```

**Key insight:** The cache key is `(root_cid, page_num)`, not just `page_num`. This is correct snapshot isolation: two different snapshots (root CIDs) of the same page number are different cache entries. A write that creates a new root CID automatically invalidates nothing — old entries remain valid for readers pinned to the old root.

#### How CID-VFS Bridges obj and sql

```
craftec-sql (libsql database)
    │
    │ xRead(page_num) / xWrite(page_num, data) / xSync()
    │
craftec-vfs (CidVfs)
    │
    │ store.put(page_data) / store.get(cid)
    │
craftec-obj (ContentAddressedStore)
    │
    │ file at data/obj/<shard>/<cid-hex>
    │
filesystem
```

**No circular dependency:** `craftec-sql` depends on `craftec-vfs` which depends on `craftec-obj`. `craftec-obj` has no knowledge of SQL. `craftec-vfs` has no knowledge of SQL semantics — it only knows about pages and CIDs.

#### Tests Required

Existing 7 tests cover the core read/write/commit/snapshot cycle. Add:
- `sequential_page_prefetch` — write pages 1–10, read page 5, verify pages 6–10 are prefetched
- `concurrent_readers_snapshot_isolation` — two snapshots at different root CIDs see different data
- `rebuild_from_disk` — create CidVfs, commit, create new CidVfs from same store, verify root CID loads correctly
- `xread_zero_fill_on_short_read` — critical per SQLite spec: `read_page()` for an unallocated page must return zero-filled data, not an error (SQLite allocates pages lazily)

---

## PART 6: PHASE 5 — DISTRIBUTED DATABASE (craftec-sql)

### craftec-sql

**Current state:** `execute()` fakes SQL by writing the SQL string as raw page bytes. `query()` always returns `Vec::new()`. Ownership checks, CAS versioning, and RPC write signature verification are fully implemented and correct.

**What needs to be built:** libsql integration with CID-VFS as the storage backend.

#### Dependency Structure

```toml
[dependencies]
craftec-types = { workspace = true }
craftec-crypto = { workspace = true }
craftec-obj = { workspace = true }
craftec-vfs = { workspace = true }
libsql = { workspace = true }    # ADD THIS — currently not in craftec-sql/Cargo.toml
tokio = { workspace = true }
blake3 = { workspace = true }
tracing = { workspace = true }
thiserror = { workspace = true }
serde = { workspace = true }
postcard = { workspace = true }
parking_lot = { workspace = true }
```

#### libsql Integration via CID-VFS

The core work is implementing a SQLite VFS plugin that routes page I/O through `CidVfs`. libsql (0.9) exposes SQLite's VFS API via Rust bindings.

**Step 1: Implement the SQLite VFS trait**

```rust
// This is the core integration work — not yet written
use libsql::ffi as sqlite3_ffi;

struct CraftecVfs {
    vfs: Arc<CidVfs>,
    runtime: tokio::runtime::Handle,
}

// SQLite VFS method implementations:
// xOpen → return CraftecVfsFile
// xDelete → no-op (content-addressed, no deletion)
// xAccess → check if root CID exists
// xFullPathname → return path as-is

struct CraftecVfsFile {
    vfs: Arc<CidVfs>,
    runtime: tokio::runtime::Handle,
}

// SQLite file method implementations:
// xRead(buf, amount, offset) → page_num = offset / page_size → vfs.read_page(page_num)
// xWrite(buf, amount, offset) → page_num = offset / page_size → vfs.write_page(page_num, buf)
// xSync(flags) → vfs.commit() → new root CID → trigger async RLNC encode
// xLock/xUnlock/xCheckReservedLock → no-ops (single-writer)
// xFileSize → root CID page count * page_size
// xClose → no-op
// xDeviceCharacteristics → SQLITE_IOCAP_ATOMIC16K | SQLITE_IOCAP_POWERSAFE_OVERWRITE
```

**Step 2: Initialize libsql with the custom VFS**

```rust
pub async fn create(owner: NodeId, vfs: Arc<CidVfs>) -> Result<Self> {
    // Register the CraftecVfs with SQLite
    let db = libsql::Builder::new_local(":memory:")
        .vfs(Box::new(CraftecVfs::new(vfs.clone())))
        .build()
        .await
        .map_err(|e| SqlError::SqlSyntaxError(e.to_string()))?;

    // Set page size before any other operations
    let conn = db.connect()?;
    conn.execute("PRAGMA page_size = 16384", ()).await?;
    conn.execute("PRAGMA journal_mode = DELETE", ()).await?;  // NOT WAL — see §35

    // First commit establishes initial root CID
    let root = vfs.commit().await?;
    Ok(Self { db_id: Cid::from_data(owner.as_bytes()), owner, vfs, root_cid: RwLock::new(root) })
}
```

**Step 3: Execute SQL through libsql**

```rust
// CURRENT (STUBBED):
pub async fn execute(&self, sql: &str, writer: &NodeId) -> Result<()> {
    self.check_ownership(&CommitContext { writer: *writer, sql: sql.to_string(), expected_root: None })?;
    // Fake: write SQL string as page data
    let page_num = u32::from_le_bytes(blake3::hash(sql.as_bytes()).as_bytes()[..4].try_into().unwrap());
    self.vfs.write_page(page_num, &padded_sql_bytes)?;
    let new_root = self.vfs.commit().await?;
    *self.root_cid.write() = new_root;
    Ok(())
}

// NEEDED:
pub async fn execute(&self, sql: &str, writer: &NodeId) -> Result<()> {
    self.check_ownership(&CommitContext { writer: *writer, sql: sql.to_string(), expected_root: None })?;
    let conn = self.db.connect().map_err(|e| SqlError::SqlSyntaxError(e.to_string()))?;
    // libsql execute → triggers xWrite → xSync → CidVfs.commit() happens inside xSync
    conn.execute(sql, libsql::params![]).await
        .map_err(|e| SqlError::SqlSyntaxError(e.to_string()))?;
    // After xSync triggers commit(), root_cid is updated inside CidVfs
    *self.root_cid.write() = self.vfs.current_root().ok_or(SqlError::NotInitialised)?;
    Ok(())
}
```

**Step 4: Query through libsql**

```rust
// CURRENT (STUBBED):
pub fn query(&self, sql: &str) -> Result<Vec<Row>> {
    let _snapshot = self.vfs.snapshot()?;  // pins root CID (correct)
    Ok(Vec::new())  // always empty
}

// NEEDED:
pub async fn query(&self, sql: &str) -> Result<Vec<Row>> {
    let snapshot = self.vfs.snapshot()?;  // pins current root CID
    let conn = self.db.connect().map_err(|e| SqlError::DatabaseError(e.to_string()))?;
    // libsql query → triggers xRead → CidVfs.read_page() → fetches from CraftOBJ
    let mut rows = conn.query(sql, libsql::params![]).await
        .map_err(|e| SqlError::SqlSyntaxError(e.to_string()))?;

    let mut result = Vec::new();
    while let Some(row) = rows.next().await? {
        let mut col_values = Vec::new();
        for i in 0..row.column_count() {
            let val = match row.get_value(i)? {
                libsql::Value::Null => ColumnValue::Null,
                libsql::Value::Integer(n) => ColumnValue::Integer(n),
                libsql::Value::Real(f) => ColumnValue::Real(f),
                libsql::Value::Text(s) => ColumnValue::Text(s.to_string()),
                libsql::Value::Blob(b) => ColumnValue::Blob(b.to_vec()),
            };
            col_values.push(val);
        }
        result.push(col_values);
    }
    drop(snapshot);  // release root CID pin
    Ok(result)
}
```

#### WAL Elimination (spec §35)

SQLite WAL (Write-Ahead Log) mode is incompatible with distributed CAS storage. Reasons:
1. WAL maintains a separate `-wal` file with unflushed writes
2. CAS requires atomic commit: the entire database state is a single CID after each commit
3. WAL's "readers access old pages while writer writes new pages" model is replaced by CID immutability

Set `PRAGMA journal_mode = DELETE` (rollback journal, not WAL). The VFS `xSync()` is the commit trigger — it flushes dirty pages to CraftOBJ and updates the root CID atomically. Journal operations (`xLock` on journal file, journal read/write) must be no-ops in the VFS implementation.

**MVCC via CID immutability:** Snapshot isolation is free — readers pin a root CID, which references immutable page CIDs. Even if the writer produces a new root CID mid-query, the reader's pinned CIDs are unchanged. No WAL needed, no rollback needed.

#### Single-Writer Commit Flow

```
SIGNED_WRITE received
       │
       ▼
verify_signature(msg) ────────► InvalidSignature error
       │
       ▼
check_ownership(writer, owner) ► UnauthorizedWriter error
       │
       ▼
check_cas(expected_root, current_root) ► CasConflict error (retry!)
       │
       ▼
db.execute(sql, writer)   ← libsql → CidVfs → CraftOBJ (sync path)
       │
       ▼
new_root_cid = vfs.current_root()
       │
       ▼
return new_root_cid to caller
       │ (async, off critical path)
       ▼
enqueue RLNC encode → distribute → announce via DHT + Pkarr + SWIM
```

#### RPC Write Path (spec §16)

The `RpcWriteHandler` is fully implemented. The signed payload format:

```rust
pub fn build_signed_payload(writer: &NodeId, sql: &str, expected_root: Option<Cid>) -> Vec<u8> {
    // u32 LE len_writer || 32 bytes writer_pubkey
    // u32 LE len_sql || sql_bytes
    // 1 byte: 0x01 if expected_root present, 0x00 if None
    // if 0x01: 32 bytes CID
}
```

The client signs this payload with their Ed25519 private key. The node verifies the signature before executing. This is "blockchain transactions" pattern: the client doesn't need to remain connected — fire and forget.

**Optimistic concurrency:** If `expected_root` doesn't match current root, return `CasConflict`. The client retries:
1. Fetch current root CID via DHT/Pkarr
2. Re-sign with new `expected_root`
3. Re-submit

This handles the case where two clients submit concurrent writes to different nodes. First write wins; second gets `CasConflict` and retries.

#### Root CID Publication (spec §37)

After each successful commit, publish the new root CID:

```rust
async fn publish_root_cid(db_id: Cid, new_root: Cid, endpoint: &CraftecEndpoint) {
    // 1. DHT provider record: "I hold content for root CID X"
    endpoint.dht().announce_provider(&new_root, &endpoint.node_id().await);

    // 2. SWIM gossip: immediate notification to connected peers
    let msg = WireMessage::ProviderAnnounce { cid: new_root, node_id: endpoint.node_id() };
    for peer in endpoint.swim().alive_members() {
        let _ = endpoint.send_message(&peer, &msg).await;
    }

    // 3. Pkarr record update (out-of-scope for initial implementation)
    // Pkarr: {name: "db.{node_id}.craftec", value: new_root_hex, ttl: 3600}
}
```

**SQL cannot route CIDs (no circular dependency):** The SQL crate has no knowledge of networking. Root CID publication is the responsibility of `craftec-node`, which subscribes to `PageCommitted` events from the event bus and calls the networking layer.

#### Schema Management

Schema migration is trivial in the single-writer model:

```rust
pub async fn migrate(db: &CraftDatabase, owner: &NodeId, migration_sql: &str) -> Result<()> {
    validate_migration_sql(migration_sql)?;
    db.execute(migration_sql, owner).await
        .map_err(|e| SqlError::MigrationFailed(e.to_string()))
}

pub fn validate_migration_sql(migration_sql: &str) -> Result<()> {
    if migration_sql.trim().is_empty() {
        return Err(SqlError::MigrationFailed("empty SQL".to_string()));
    }
    // Warn if no DDL keywords (may be DML-only migration)
    Ok(())
}
```

Schema changes produce a new root CID, like any other write. Readers discover schema changes via a `schema_version` field in the page index (not yet implemented in current code).

#### Step-by-Step Implementation Plan

1. Add `libsql = { workspace = true }` to `craftec-sql/Cargo.toml`
2. Implement `CraftecVfs` struct implementing SQLite VFS methods
3. Implement `CraftecVfsFile` struct implementing SQLite file methods
4. Register VFS with libsql `Builder`
5. Update `CraftDatabase::create()` to use the VFS builder
6. Update `execute()` to call `conn.execute()` instead of fake page writes
7. Update `query()` to call `conn.query()` and materialize rows
8. Add integration tests: INSERT → query → verify row exists
9. Add CAS conflict test: two concurrent writes, verify one succeeds and one gets `CasConflict`
10. Add snapshot isolation test: reader pins root CID before writer commits, sees old data

---

## PART 7: PHASE 6 — NETWORKING (craftec-net)

### craftec-net

**Current state:** iroh Endpoint scaffolded with both ALPNs registered. SWIM state machine is fully implemented but probes are not dispatched over the network. DHT is in-memory only with no gossip/TTL/Kademlia. Handler replies are discarded.

**What needs to be built:** Wire up SWIM probe dispatch, implement real message routing, add DHT gossip propagation.

#### iroh 0.96 Endpoint Setup

iroh 0.96 merged `iroh-net` into `iroh`. The main type is `iroh::Endpoint`. Key API change from earlier versions: `Endpoint::builder()` replaces the old constructors.

```rust
use iroh::{Endpoint, SecretKey, NodeAddr};

pub const ALPN_CRAFTEC: &[u8] = b"craftec/0.1";
pub const ALPN_SWIM: &[u8] = b"craftec-swim/0.1";

async fn build_endpoint(keypair: &NodeKeypair) -> Result<Endpoint> {
    // iroh NodeId IS the Ed25519 public key — same bytes as our NodeId
    let secret_bytes = keypair.to_secret_bytes();
    let secret_key = SecretKey::from_bytes(&secret_bytes);

    let endpoint = Endpoint::builder()
        .secret_key(secret_key)
        .alpns(vec![ALPN_CRAFTEC.to_vec(), ALPN_SWIM.to_vec()])
        .bind()
        .await
        .map_err(|e| NetError::ConnectionFailed {
            peer: "self".to_string(),
            reason: e.to_string(),
        })?;

    Ok(endpoint)
}
```

**iroh 0.96 key facts:**
- `Endpoint::builder()` returns `Builder`
- `.alpns(vec![...])` registers accepted ALPNs
- `.bind().await` starts listening
- `endpoint.node_id()` returns `iroh::PublicKey` (= our `NodeId` bytes)
- `endpoint.node_addr().await` returns full `NodeAddr` including relay URL
- `endpoint.connect(node_addr, alpn)` opens a connection to a peer
- Connections are `iroh::endpoint::Connection`; streams are `SendStream`/`RecvStream`

#### Connection Lifecycle

```rust
pub struct CraftecEndpoint {
    endpoint: iroh::Endpoint,
    node_id: NodeId,
    connections: Arc<ConnectionPool>,
    swim: Arc<SwimMembership>,
}

impl CraftecEndpoint {
    pub async fn new(config: &NodeConfig, keypair: &NodeKeypair) -> Result<Self> {
        let endpoint = build_endpoint(keypair).await?;
        let node_id = keypair.node_id();
        Ok(Self {
            endpoint,
            node_id,
            connections: Arc::new(ConnectionPool::new()),
            swim: Arc::new(SwimMembership::new(node_id)),
        })
    }

    /// Open a new connection to a peer (or return pooled connection).
    pub async fn get_or_connect(&self, peer: &NodeId) -> Result<iroh::endpoint::Connection> {
        if let Some(conn) = self.connections.get(peer) {
            return Ok(conn);
        }
        // Build NodeAddr from peer bytes — requires relay URL or direct address
        // In practice, resolve via DHT/Pkarr first
        let addr = NodeAddr::new(iroh::PublicKey::from_bytes(peer.as_bytes())?);
        let conn = self.endpoint.connect(addr, ALPN_CRAFTEC).await
            .map_err(|e| NetError::ConnectionFailed {
                peer: peer.to_string(),
                reason: e.to_string(),
            })?;
        self.connections.insert(*peer, conn.clone());
        Ok(conn)
    }

    /// Send a WireMessage to a peer using a unidirectional stream.
    pub async fn send_message(&self, peer: &NodeId, msg: &WireMessage) -> Result<()> {
        let conn = self.get_or_connect(peer).await?;
        let mut send = conn.open_uni().await
            .map_err(|e| NetError::ProtocolError(e.to_string()))?;
        let bytes = craftec_types::wire::encode(msg)
            .map_err(|e| NetError::SerializationError(e.to_string()))?;
        // Length-prefix framing
        let len = bytes.len() as u32;
        send.write_all(&len.to_le_bytes()).await
            .map_err(|e| NetError::Io(e))?;
        send.write_all(&bytes).await
            .map_err(|e| NetError::Io(e))?;
        send.finish().await
            .map_err(|e| NetError::ProtocolError(e.to_string()))?;
        Ok(())
    }
}
```

#### Wire Protocol Framing

The spec (§23) defines a 9-byte header: `[type_tag: u32 | version: u8 | payload_len: u32]`. Current implementation uses only a 4-byte length prefix. For full spec compliance:

```rust
const WIRE_VERSION: u8 = 1;

fn encode_framed(msg: &WireMessage) -> Result<Vec<u8>> {
    let payload = craftec_types::wire::encode(msg)?;
    let type_tag = msg_type_tag(msg);
    let mut frame = Vec::with_capacity(9 + payload.len());
    frame.extend_from_slice(&type_tag.to_le_bytes());   // 4 bytes
    frame.push(WIRE_VERSION);                            // 1 byte
    frame.extend_from_slice(&(payload.len() as u32).to_le_bytes()); // 4 bytes
    frame.extend_from_slice(&payload);
    Ok(frame)
}

fn msg_type_tag(msg: &WireMessage) -> u32 {
    match msg {
        WireMessage::Ping { .. } => 0x01000002,
        WireMessage::Pong { .. } => 0x01000003,
        WireMessage::PieceRequest { .. } => 0x02000001,
        WireMessage::PieceResponse { .. } => 0x02000004,
        WireMessage::SwimJoin { .. } => 0x03000001,
        WireMessage::SignedWrite { .. } => 0x06000001,
        // etc.
    }
}
```

#### Accept Loop

```rust
pub async fn accept_loop<H>(&self, handler: Arc<H>)
where H: ConnectionHandler + 'static
{
    loop {
        match self.endpoint.accept().await {
            Some(incoming) => {
                let handler = handler.clone();
                let swim = self.swim.clone();
                let pool = self.connections.clone();
                tokio::spawn(async move {
                    match incoming.await {
                        Ok(conn) => {
                            let remote = conn.remote_node_id();
                            let alpn = conn.alpn();
                            match alpn.as_slice() {
                                a if a == ALPN_CRAFTEC => {
                                    // Cache connection
                                    let node_id = NodeId::from_bytes(remote.as_bytes());
                                    pool.insert(node_id, conn.clone());
                                    handle_craftec_conn(conn, node_id, handler).await;
                                }
                                a if a == ALPN_SWIM => {
                                    let node_id = NodeId::from_bytes(remote.as_bytes());
                                    handle_swim_conn(conn, node_id, swim).await;
                                }
                                other => {
                                    tracing::warn!("Unknown ALPN: {:?}", other);
                                }
                            }
                        }
                        Err(e) => tracing::warn!("Accept error: {}", e),
                    }
                });
            }
            None => break,  // endpoint closed
        }
    }
}

async fn handle_craftec_conn<H>(
    conn: iroh::endpoint::Connection,
    remote: NodeId,
    handler: Arc<H>,
) where H: ConnectionHandler {
    loop {
        match conn.accept_uni().await {
            Ok(mut recv) => {
                // Read length-prefix
                let mut len_buf = [0u8; 4];
                if recv.read_exact(&mut len_buf).await.is_err() { break; }
                let len = u32::from_le_bytes(len_buf) as usize;
                if len > 4 * 1024 * 1024 { break; }  // 4 MB max message
                let mut buf = vec![0u8; len];
                if recv.read_exact(&mut buf).await.is_err() { break; }
                match craftec_types::wire::decode(&buf) {
                    Ok(msg) => {
                        // Handler returns optional reply
                        if let Some(reply) = handler.handle_message(remote, msg).await {
                            // Open a new uni stream to send the reply
                            // TODO: send reply back to remote
                        }
                    }
                    Err(e) => tracing::warn!("Decode error from {}: {}", remote, e),
                }
            }
            Err(_) => break,
        }
    }
}
```

#### SWIM Membership Protocol

The `SwimMembership` state machine is fully implemented. What's missing is the network dispatch:

```rust
// CURRENT (broken) — probes are computed but not sent
pub async fn run_swim_loop(swim: Arc<SwimMembership>, mut shutdown: broadcast::Receiver<()>) {
    let mut interval = tokio::time::interval(swim.protocol_period);
    loop {
        tokio::select! {
            _ = interval.tick() => {
                let _probes = swim.protocol_tick().await;  // probes DISCARDED
            }
            _ = shutdown.recv() => break,
        }
    }
}

// NEEDED — probes must be dispatched via network
pub async fn run_swim_loop(
    swim: Arc<SwimMembership>,
    endpoint: Arc<CraftecEndpoint>,   // ADD THIS
    mut shutdown: broadcast::Receiver<()>,
) {
    let mut interval = tokio::time::interval(swim.protocol_period);
    loop {
        tokio::select! {
            _ = interval.tick() => {
                let probes = swim.protocol_tick().await;
                for (target_node, probe_msg) in probes {
                    let ep = endpoint.clone();
                    tokio::spawn(async move {
                        if let Err(e) = ep.send_message(&target_node, &probe_msg).await {
                            tracing::debug!("SWIM probe to {} failed: {}", target_node, e);
                            // Don't panic — SWIM failure detection handles unreachable nodes
                        }
                    });
                }
            }
            _ = shutdown.recv() => break,
        }
    }
}
```

**SWIM parameters from spec §13:**
- Round interval: 500ms (currently 1s in code — update `protocol_period`)
- Indirect probe fanout: K=3 (not yet implemented — `protocol_tick` only sends direct PING)
- Suspicion timeout: 3 rounds = 1.5s (currently 10s in code — update `suspect_timeout`)

**Indirect probe (not yet implemented):**
When a direct PING gets no ACK within one round, ask K=3 random members to send `PING-REQ` to the suspect. If none of them gets an ACK either, mark as suspected. This distinguishes "dead" from "I can't reach it". Without indirect probes, a single network partition looks like a node failure.

#### DHT Provider Records

The current `DhtProviders` is in-memory with no persistence or gossip. Per spec §18, the DHT uses Kademlia for provider record storage:

```rust
// CURRENT: in-memory HashMap only
pub struct DhtProviders {
    local_providers: DashMap<Cid, HashSet<NodeId>>,
}

// NEEDED additions:
// 1. Gossip propagation of ProviderAnnounce messages
// 2. TTL-based expiry (24h per spec §18)
// 3. Re-announcement timer (every 22h)
// 4. iroh DHT integration when available

// Gossip approach (works today without Kademlia):
impl CraftecEndpoint {
    pub async fn announce_cid_to_peers(&self, cid: &Cid) {
        let msg = WireMessage::ProviderAnnounce {
            cid: *cid,
            node_id: self.node_id,
        };
        // Announce to log(N) random alive peers (epidemic gossip)
        let peers = self.swim.alive_members();
        let fanout = (peers.len() as f64).log2().ceil() as usize;
        let selected: Vec<_> = peers.choose_multiple(&mut rand::thread_rng(), fanout).collect();
        for peer in selected {
            let _ = self.send_message(peer, &msg).await;
        }
    }
}
```

**iroh DHT note (from code audit):** "iroh 0.96 does not ship a built-in content-routing DHT." The iroh team is working on `iroh-dht-experiment`. For now, use epidemic gossip via SWIM piggybacking as the provider record propagation mechanism. This is sufficient for correctness (every node that holds a CID will gossip it to log(N) peers), though latency is higher than Kademlia.

#### Connection Pool Management

```rust
pub struct ConnectionPool {
    connections: Arc<DashMap<NodeId, PooledConnection>>,
    max_connections: usize,  // default: 256; spec says 200 + 20 reserved for inbound
}
```

**Eviction policy (spec §25):**
- Global max: 200 connections (20 reserved for inbound)
- Every 300s: if >90% full, disconnect bottom 4% by reliability score
- Connection score = 0.6 × reliability + 0.2 × uptime + 0.2 × (1 - latency_normalized)
- SWIM-dead filter: before connecting to a DHT-returned provider, check if they're in the dead set

**Currently missing:** LRU eviction in `insert()` removes the oldest `last_used`, not the lowest score. Add a reliability score to `PooledConnection` for proper score-based eviction.

#### NAT Traversal

iroh handles NAT traversal automatically:
1. All connections start via relay (immediate connectivity)
2. iroh attempts DCUtR hole-punch upgrade (70% success)
3. Falls back to relay permanently for symmetric NAT (~30% of users)

Configuration — no special setup needed. Just use `Endpoint::builder()`. The relay URL is discovered via iroh's relay infrastructure. For private networks, configure a custom relay:

```rust
Endpoint::builder()
    .relay_mode(iroh::RelayMode::Custom(relay_url))
    // ...
```

#### Message Routing Dispatch

The `ConnectionHandler` trait:

```rust
pub trait ConnectionHandler: Send + Sync + 'static {
    fn handle_message(&self, from: NodeId, msg: WireMessage) -> HandlerFuture;
}
// HandlerFuture = Pin<Box<dyn Future<Output = Option<WireMessage>> + Send + 'static>>
```

The `NullHandler` (current implementation) discards all messages. The real handler (to be built in `craftec-node`) routes:

```
PieceRequest       → serve coded pieces from ContentAddressedStore
PieceResponse      → feed to waiting RlncDecoder (via pending_fetches channel)
ProviderAnnounce   → record in DhtProviders
HealthReport       → record in PieceTracker
SignedWrite        → forward to RpcWriteHandler
Ping               → reply with Pong
SwimJoin/Alive/Suspect/Dead → handled by SwimMembership (already wired for SWIM ALPN)
```

#### Step-by-Step: First Two Nodes Talking

1. Node A starts: `CraftecEndpoint::new()` → iroh endpoint listening
2. Node B starts: same
3. Node B's config has Node A's NodeId as `bootstrap_peer`
4. `endpoint.bootstrap(["nodeA_pubkey_hex"])` → `iroh::Endpoint::connect()` → QUIC handshake
5. Bootstrap sends `WireMessage::SwimJoin { node_id: B, listen_port: 0 }`
6. Node A's accept loop receives the SwimJoin, routes to `handle_swim_conn`
7. `swim.handle_message(SwimJoin)` → A adds B to membership as Alive
8. Node A's SWIM loop ticks → sends `SwimPing` to B
9. Node B receives SwimPing → updates A's membership
10. Both nodes see each other as Alive in SWIM

To verify: `swim.alive_members()` should return each other's NodeIds.

#### Step-by-Step: Add Piece Exchange

1. Node A encodes data: `rlnc.encode(data, 32).await`
2. Node A stores coded pieces: for each piece, `store.put(piece_bytes).await`
3. Node A announces: `announce_cid_to_peers(&data_cid).await`
4. Node B receives `ProviderAnnounce` → records in `DhtProviders`
5. Node B wants data: `dht.get_providers(&data_cid)` → [NodeA]
6. Node B sends `WireMessage::PieceRequest { cid, piece_indices: vec![] }` to Node A
7. Node A receives PieceRequest → fetches pieces from store → sends `PieceResponse`
8. Node B receives PieceResponse → feeds pieces to `RlncDecoder`
9. After k independent pieces: `decoder.decode()` → original data

---

## PART 8: PHASE 7 — HEALTH & REPAIR (craftec-health)

### craftec-health

**Current state:** Natural selection coordinator (DONE), PieceTracker (DONE), HealthScanner scan logic (DONE). Scanner forwards RepairRequests to RepairExecutor via mpsc channel.

**What needs to be built:** Parallel repair (top-N election), local recode from own pieces, distribution priority (1-piece holders first).

#### HealthScan: 1% Per Cycle, Full Coverage Every ~8 Hours

**Scan eligibility:** Only nodes holding ≥2 coded pieces for a CID participate in health scanning for that CID. This is because ≥2 pieces are required to recode. If a node holds only 1 piece, it cannot repair — it can only be a *distribution target* for a newly recoded piece.

```rust
pub struct HealthScanner {
    store: Arc<ContentAddressedStore>,
    scan_percent: f64,           // 0.01 (1%)
    interval: Duration,          // 5 minutes
    piece_tracker: Arc<PieceTracker>,
    last_scan_index: AtomicUsize,
}

impl HealthScanner {
    /// One scan cycle: take 1% of tracked CIDs starting from cursor, evaluate each.
    /// Only CIDs where this node holds ≥2 pieces are eligible for repair evaluation.
    pub async fn scan_cycle(&self) -> Result<Vec<RepairRequest>> {
        let sorted_cids = self.piece_tracker.sorted_cids();  // deterministic order
        let total = sorted_cids.len();
        if total == 0 { return Ok(vec![]); }

        let batch_size = ((total as f64 * self.scan_percent).ceil() as usize).max(1);
        let start = self.last_scan_index.load(Ordering::Acquire) % total;
        let end = (start + batch_size).min(total);

        let mut repair_needed = Vec::new();
        for cid in &sorted_cids[start..end] {
            // Only evaluate CIDs where this node holds ≥2 pieces (can recode)
            let local_pieces = self.piece_tracker.local_piece_count(cid);
            if local_pieces < 2 { continue; }

            if let Some(req) = self.evaluate_cid(cid) {
                repair_needed.push(req);
            }
        }

        // Advance cursor (wrap around for continuous coverage)
        self.last_scan_index.store((start + batch_size) % total, Ordering::Release);
        Ok(repair_needed)
    }

    fn evaluate_cid(&self, cid: &Cid) -> Option<RepairRequest> {
        // Query DHT for total coded piece count across the network
        let available = self.piece_tracker.available_count(cid);
        let k = self.piece_tracker.get_k(cid).unwrap_or(DEFAULT_K);
        // Target = k × (2.0 + 16/k) = 2k + 16
        let target = target_piece_count(k);

        if available < k {
            Some(RepairRequest::Critical { cid: *cid, available, k })
        } else if available < target {
            Some(RepairRequest::Normal { cid: *cid, available, target })
        } else {
            None
        }
    }
}

/// Target piece count: n = k × redundancy(k) = k × (2.0 + 16/k) = 2k + 16.
/// K=8 → n=32 coded pieces.  K=32 → n=80 coded pieces.
fn target_piece_count(k: u32) -> u32 {
    2 * k + 16
}
```

**Coverage calculation:** 1% per cycle × 5-minute interval = 100 cycles for full coverage. 100 × 5 min = 500 min ≈ 8.3 hours. Exactly matches spec §30: "100% of held CIDs covered per full pass... full coverage every ~8 hours."

**CID ordering:** `sorted_cids()` returns CIDs sorted by their byte value. Combined with a rolling cursor, this ensures every CID is scanned within 100 cycles regardless of which CIDs are added/removed between cycles.

#### Tracker: Per-CID Coded Piece Count

**RLNC key property:** Every coded piece is unique — each has its own random GF(2^8) coefficient vector. There are no "piece indices" as in Reed-Solomon. The tracker counts how many distinct nodes hold coded pieces for each CID across the network.

```rust
pub struct PieceHolder {
    pub node_id: NodeId,
    pub last_seen: Instant,
}

pub struct PieceTracker {
    /// Cid → list of nodes holding coded pieces for this CID.
    availability: Arc<DashMap<Cid, Vec<PieceHolder>>>,
    /// Cid → K value used for RLNC encoding (variable-K support).
    k_values: Arc<DashMap<Cid, u32>>,
}

impl PieceTracker {
    /// Record that node_id holds a coded piece for cid.
    /// Updates last_seen if the node already has a record.
    pub fn record_piece(&self, cid: &Cid, holder: PieceHolder)

    /// Total coded piece count across the network for this CID.
    /// When count < target (2k+16) → under-replicated.
    pub fn available_count(&self, cid: &Cid) -> u32

    /// How many pieces THIS node holds locally for a CID.
    /// Needed to determine scan eligibility (≥2 required for recode).
    pub fn local_piece_count(&self, cid: &Cid) -> u32

    pub fn get_holders(&self, cid: &Cid) -> Vec<PieceHolder>

    /// Remove all holder records for a dead node.
    pub fn remove_node(&self, node_id: &NodeId)

    /// Remove stale records (node hasn't re-announced within max_age).
    pub fn prune_stale(&self, max_age: Duration) -> usize

    /// Sorted by CID bytes — for deterministic scanner cursor.
    pub fn sorted_cids(&self) -> Vec<Cid>

    /// Record the K value used for encoding this CID (first-writer-wins).
    pub fn record_k(&self, cid: &Cid, k: u32)
    pub fn get_k(&self, cid: &Cid) -> Option<u32>
}
```

**Population:** The PieceTracker is populated from:
1. `HealthReport` messages received from peers (inbound message routing)
2. `ProviderAnnounce` messages (when a node announces it holds pieces for a CID)
3. Local: when this node stores a coded piece, record itself as a holder

#### Natural Selection Coordinator — Parallel Repair

**Key insight:** Repair is NOT single-coordinator. When a CID is short by N pieces, the top-N ranked nodes **all** produce 1 new piece each in the same cycle. This is parallel self-coordinating repair — no bottleneck, no election protocol.

```rust
pub struct NodeRanking {
    pub node_id: NodeId,
    pub uptime_secs: u64,
    pub reputation_score: f64,  // 0.0–1.0
}

pub struct NaturalSelectionCoordinator;

impl NaturalSelectionCoordinator {
    /// Rank all candidate nodes by quality.
    /// Ranking: uptime DESC, reputation DESC, NodeId ASC (deterministic tiebreaker)
    /// All nodes compute the same ranking independently — no election needed.
    pub fn rank_providers(providers: &[NodeRanking]) -> Vec<NodeId>

    /// Determine how many nodes should repair (= deficit = target - current_count).
    /// The top N ranked nodes are all elected to produce 1 piece each.
    pub fn elected_repairers(
        providers: &[NodeRanking],
        deficit: u32,
    ) -> Vec<NodeId>
}
```

**Parallel repair mechanism:**
1. Deficit = target (2k+16) − current coded piece count
2. All holders with ≥2 pieces are candidates, ranked by (uptime, reputation, NodeID)
3. Top N (= deficit) ranked nodes are all elected
4. Each node independently computes whether it is in the top N
5. Each elected node produces 1 new piece via local recode → N pieces repaired per cycle
6. "1 piece per CID per cycle" is **per elected node**, not network-wide

**Critical design point (from spec §30, project owner constraint):** Coordinator selection is by quality (uptime/reputation/NodeID), NOT by XOR distance from the CID. RLNC pieces are distributed randomly, not by key-space proximity.

**No network synchronization needed:** All nodes holding ≥2 pieces for a CID independently compute the same ranking from the same input data (uptime, reputation, NodeId). Each determines if it's in the top-N and acts accordingly. If a top-ranked node is down (detected via SWIM), the next-ranked node takes its slot naturally.

#### Repair Flow — Local Recode + Distribute

**Critical RLNC property:** The repair node already holds ≥2 coded pieces locally (that's the scan eligibility requirement). It recodes from its own local pieces — **no network fetch needed for recode input**. The only network operation is distributing the new coded piece to a non-holder.

```rust
pub struct RepairExecutor {
    rlnc_engine: Arc<RlncEngine>,
    store: Arc<ContentAddressedStore>,
    net: Arc<CraftecEndpoint>,
    tracker: Arc<PieceTracker>,
    coordinator: NaturalSelectionCoordinator,
    our_node_id: NodeId,
}

impl RepairExecutor {
    pub async fn execute_repair(&self, request: &RepairRequest) -> Result<()> {
        let cid = request.cid();
        let holders = self.tracker.get_holders(cid);

        // Step 1: Am I elected to repair?
        // Compute deficit, rank all holders with ≥2 pieces, check if we're in top-N.
        let target_count = target_piece_count(
            self.tracker.get_k(cid).unwrap_or(DEFAULT_K)
        );
        let current_count = self.tracker.available_count(cid);
        let deficit = target_count.saturating_sub(current_count);
        if deficit == 0 { return Ok(()); }

        let rankings = self.build_rankings(&holders);
        let elected = NaturalSelectionCoordinator::elected_repairers(&rankings, deficit);
        if !elected.contains(&self.our_node_id) {
            return Ok(()); // Another node handles it
        }

        // Step 2: Load ≥2 coded pieces from LOCAL store (no network fetch)
        let local_pieces = self.store.get_local_pieces(cid, 2)?;
        if local_pieces.len() < 2 {
            return Err(HealthError::InsufficientPieces { ... });
        }

        // Step 3: Recode — combine with fresh random GF(2^8) coefficients (no decode)
        let recoded = self.rlnc_engine.recode(&local_pieces).await?;

        // Step 4: Select distribution target
        //   Priority 1: peers with 1 piece (bring them to ≥2 so they can join future repair)
        //   Priority 2: peers with 0 pieces (non-holders)
        let target = self.select_distribution_target(cid, &holders)?;

        // Step 5: Send recoded piece to target
        let response = WireMessage::PieceResponse { pieces: vec![recoded] };
        self.net.send_message(&target, &response).await?;

        Ok(())
    }

    /// Distribution target priority:
    /// 1. Peers holding exactly 1 piece (bring them to ≥2 so they can participate in future repair)
    /// 2. Peers holding 0 pieces (non-holders — increases overall availability)
    fn select_distribution_target(
        &self,
        cid: &Cid,
        holders: &[PieceHolder],
    ) -> Result<NodeId> {
        let alive_peers = self.net.swim().alive_members();
        let holder_counts = self.tracker.per_node_piece_count(cid);

        // Priority 1: alive peers with exactly 1 piece
        let single_piece: Vec<NodeId> = alive_peers.iter()
            .filter(|id| holder_counts.get(id) == Some(&1))
            .copied().collect();
        if !single_piece.is_empty() {
            return Ok(*single_piece.choose(&mut rand::thread_rng()).unwrap());
        }

        // Priority 2: alive peers with 0 pieces
        let non_holders: Vec<NodeId> = alive_peers.iter()
            .filter(|id| !holder_counts.contains_key(id))
            .copied().collect();
        if !non_holders.is_empty() {
            return Ok(*non_holders.choose(&mut rand::thread_rng()).unwrap());
        }

        // Fallback: any alive peer (small network, everyone already holds pieces)
        alive_peers.choose(&mut rand::thread_rng())
            .copied()
            .ok_or_else(|| HealthError::RepairFailed { ... })
    }
}
```

**No `fetch_pieces()` — no `PendingFetches` for repair.** The recode input comes entirely from the local store. `PendingFetches` is only used for the **read path** (client fetching pieces to decode), not for repair.

#### Scanner → Repair Wiring

```rust
// Scanner forwards RepairRequests via mpsc channel to RepairExecutor.
pub async fn run(
    &self,
    repair_tx: mpsc::Sender<RepairRequest>,
    mut shutdown: broadcast::Receiver<()>,
) {
    loop {
        tokio::select! {
            _ = tokio::time::sleep(self.interval.div_f64(100.0)) => {
                let requests = self.scan_cycle().await.unwrap_or_default();
                for req in requests {
                    if repair_tx.send(req).await.is_err() {
                        tracing::warn!("Repair channel full or closed");
                    }
                }
            }
            _ = shutdown.recv() => break,
        }
    }
}

// In craftec-node run loop:
let (repair_tx, mut repair_rx) = mpsc::channel::<RepairRequest>(128);
tokio::spawn(async move { scanner.run(repair_tx, shutdown_rx2).await });
tokio::spawn(async move {
    while let Some(req) = repair_rx.recv().await {
        // RepairExecutor checks if this node is elected before acting
        let _ = repair_executor.execute_repair(&req).await;
    }
});
```

#### Degradation: Shedding Excess Pieces

When a node holds more pieces than needed (e.g., after network scaling back down), degradation sheds excess. This is **NOT economic** — it's purely about maintaining the right replication factor.

The degradation policy agent (WASM, §10) defines which pieces to drop. The kernel-level mechanism just provides the `delete(cid)` endpoint. Excess detection: if `available_count > target_n`, coordinator drops 1 piece per cycle (symmetric with repair: gain or lose 1 piece per cycle).

---

## PART 9: PHASE 8 — COMPUTE RUNTIME (craftec-com)

### craftec-com

**Current state:** Wasmtime `ComRuntime` executes real WASM. Agent lifecycle state machine (Loaded/Running/Stopped) works. `craft_log` host function reads WASM memory and emits tracing logs. Everything else is stubbed.

**What needs to be built:** Host function implementations, scheduler actual execution, agent auto-start.

#### Wasmtime WASI 0.2 Runtime Setup

```rust
pub struct ComRuntime {
    engine: Engine,
    pub fuel_limit: u64,  // default: 10_000_000
}

impl ComRuntime {
    pub fn new(fuel_limit: u64) -> Result<Self> {
        let mut config = Config::new();
        config.consume_fuel(true);
        config.cranelift_opt_level(OptLevel::Speed);
        // Epoch-based interruption for wall-clock timeouts:
        config.epoch_interruption(true);

        let engine = Engine::new(&config)
            .map_err(|e| ComError::RuntimeConfigError(e.to_string()))?;
        Ok(Self { engine, fuel_limit })
    }

    pub async fn execute_agent(
        &self,
        wasm_bytes: &[u8],
        entry_point: &str,
        args: &[Val],
    ) -> Result<Vec<Val>> {
        let module = Module::new(&self.engine, wasm_bytes)
            .map_err(|e| ComError::WasmCompilationFailed(e.to_string()))?;

        let mut store = Store::new(&self.engine, ());
        store.set_fuel(self.fuel_limit)
            .map_err(|e| ComError::RuntimeConfigError(e.to_string()))?;
        store.set_epoch_deadline(1);  // 1 epoch = configurable wall-clock timeout

        let mut linker = Linker::new(&self.engine);
        HostFunctions::register(&mut linker)?;

        let instance = linker.instantiate(&mut store, &module)
            .map_err(|e| ComError::WasmCompilationFailed(e.to_string()))?;

        let func = instance.get_func(&mut store, entry_point)
            .ok_or_else(|| ComError::EntryPointNotFound(entry_point.to_string()))?;

        let mut results = vec![Val::I32(0); func.ty(&store).results().len()];
        func.call(&mut store, args, &mut results).map_err(|e| {
            if e.to_string().contains("fuel") {
                ComError::FuelExhausted { consumed: self.fuel_limit, limit: self.fuel_limit }
            } else {
                ComError::Trap(e.to_string())
            }
        })?;
        Ok(results)
    }
}
```

**Memory configuration:** Wasmtime's default heap is 4 GiB address space (though only pages that are actually written use physical memory). The spec per-program memory limits (§38) are enforced via the store's `MemoryCreator` or by catching OOM traps. Per-program limits: Reputation scorer: 64 MB; unknown programs: 256 MB ceiling.

**Fuel calibration (spec §38):** Run a calibration benchmark on startup to measure fuel-per-wall-second on the actual hardware. Store the calibration factor. Translate CPU budget (e.g., "500ms") to fuel units using the factor. Alternative: use epoch interruption (wall-clock based) as primary timeout, fuel as secondary.

#### Host Function Implementations

The key challenge: host functions need access to `ContentAddressedStore` and `CraftDatabase`. The current `Linker<()>` uses unit type as the state — change to a proper `HostState`:

```rust
pub struct HostState {
    pub store: Arc<ContentAddressedStore>,
    pub database: Arc<CraftDatabase>,
    pub keystore: Arc<KeyStore>,
    pub call_counts: HashMap<String, u32>,
}

// Update ComRuntime to use Linker<HostState>
```

**`craft_store_get(ptr: i32, len: i32) → i64`**

```rust
fn register_store_get(linker: &mut Linker<HostState>) -> Result<(), ComError> {
    linker.func_wrap("craftec", "craft_store_get", |mut caller: Caller<HostState>, ptr: i32, len: i32| -> i64 {
        // Read CID bytes from WASM memory
        let memory = caller.get_export("memory")
            .and_then(|e| e.into_memory())
            .ok_or(-1i64)?;
        let mut cid_bytes = [0u8; 32];
        memory.read(&caller, ptr as usize, &mut cid_bytes).ok()?;
        let cid = Cid::from_bytes(cid_bytes);

        // Fetch from CraftOBJ (blocking via tokio::runtime::Handle)
        let store = caller.data().store.clone();
        let rt = tokio::runtime::Handle::current();
        match rt.block_on(store.get(&cid)) {
            Ok(Some(data)) => {
                // Write data into WASM memory scratch buffer (at well-known offset)
                // Returns: data length on success, -1 on error
                // In practice: allocate scratch space via guest-exported `alloc` function
                data.len() as i64
            }
            Ok(None) => -2,  // Not found
            Err(_) => -1,    // Error
        }
    })?;
    Ok(())
}
```

**WASM memory access pattern:** The guest WASM module must export a scratch buffer (e.g., via a `alloc(size) -> ptr` function) where the host writes returned data. This requires a convention between the host runtime and the WASM guest. For MVP, allocate a fixed scratch region (e.g., at memory offset 0x10000) and document the ABI.

**`craft_sign(ptr: i32, len: i32) → i64`**

```rust
fn register_sign(linker: &mut Linker<HostState>) -> Result<(), ComError> {
    linker.func_wrap("craftec", "craft_sign", |mut caller: Caller<HostState>, ptr: i32, len: i32| -> i64 {
        let memory = caller.get_export("memory").and_then(|e| e.into_memory())?;
        let mut msg_bytes = vec![0u8; len as usize];
        memory.read(&caller, ptr as usize, &mut msg_bytes).ok()?;

        // Rate limit: max 10 calls per invocation (spec §40)
        let counts = &mut caller.data_mut().call_counts;
        let count = counts.entry("craft_sign".to_string()).or_insert(0);
        if *count >= 10 { return -3; }  // Rate limit exceeded
        *count += 1;

        let sig = caller.data().keystore.sign(&msg_bytes);
        let sig_bytes = sig.to_bytes();

        // Write 64-byte signature to WASM scratch buffer
        // Return offset where signature was written
        0i64  // TODO: write to scratch buffer
    })?;
    Ok(())
}
```

#### Agent Lifecycle: Deploy, Start, Pause, Stop, Upgrade

```rust
pub enum ProgramState {
    Loaded { wasm_cid: Cid, loaded_at: Instant },
    Running { wasm_cid: Cid, started_at: Instant },
    Stopped { wasm_cid: Cid, reason: String },
    // Add:
    Quarantined { wasm_cid: Cid, crash_count: u32 },  // spec §48
    Paused { wasm_cid: Cid },
}

pub struct ProgramScheduler {
    programs: DashMap<Cid, ProgramState>,
    runtime: Arc<ComRuntime>,
    // Add:
    crash_counts: DashMap<Cid, u32>,
    restart_backoffs: DashMap<Cid, Duration>,
}

impl ProgramScheduler {
    pub async fn start_program(&self, wasm_cid: &Cid) -> Result<()> {
        // CURRENT: just transitions state to Running, no execution
        // NEEDED: spawn background task

        let wasm_bytes = /* fetch from CraftOBJ */ vec![];
        let runtime = self.runtime.clone();
        let cid = *wasm_cid;
        let scheduler = Arc::clone(self_arc);  // need Arc<Self>

        tokio::spawn(async move {
            loop {
                match runtime.execute_agent(&wasm_bytes, "run", &[]).await {
                    Ok(_) => {
                        // Program exited normally — restart if keepalive
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                    Err(ComError::Trap(reason)) => {
                        // Crash: apply backoff, check quarantine threshold
                        let count = scheduler.increment_crash_count(&cid);
                        if count >= 10 {
                            scheduler.quarantine(&cid);
                            break;
                        }
                        let backoff = Duration::from_secs(1 << count.min(6));  // max 64s
                        tokio::time::sleep(backoff).await;
                    }
                    Err(ComError::FuelExhausted { .. }) => {
                        // Budget exceeded — restart immediately
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                    Err(e) => {
                        tracing::error!("Agent {} error: {}", cid, e);
                        break;
                    }
                }
            }
        });

        self.programs.insert(*wasm_cid, ProgramState::Running {
            wasm_cid: *wasm_cid,
            started_at: Instant::now(),
        });
        Ok(())
    }
}
```

#### Auto-Start Built-in Agents on Boot

On node startup, after CraftOBJ and CraftSQL are ready:

```rust
// In craftec-node::CraftecNode::new():
let default_agents = [
    Agent::local_eviction(Cid::from_data(b"local-eviction-v1")),
    Agent::reputation_scoring(Cid::from_data(b"reputation-scoring-v1")),
];
for agent in &default_agents {
    // Load WASM bytecode from CraftOBJ (must be pre-seeded on first boot)
    if let Ok(Some(wasm_bytes)) = store.get(&agent.wasm_cid).await {
        scheduler.load_program(&agent.wasm_cid, &wasm_bytes).await?;
        scheduler.start_program(&agent.wasm_cid).await?;
    } else {
        tracing::warn!("Built-in agent {} not found in store", agent.name);
        // Fall back to hardcoded safe defaults (spec §48)
    }
}
```

#### Resource Limits

Per program from spec §38:

| Agent | Memory | Fuel/invocation |
|---|---|---|
| Reputation scorer | 64 MB | configurable |
| Eviction policy | 32 MB | configurable |
| Agent load balancer | 16 MB | configurable |
| Degradation policy | 32 MB | configurable |
| Schema migration | 64 MB | configurable |
| Unknown/third-party | 256 MB | 10,000,000 |

Enforce memory via `Config::static_memory_maximum()` per-store, or catch OOM traps.

#### Attestation as One Possible Agent Workload

Attestation is NOT a kernel-level concern — it's one specific workload that runs on CraftCOM (spec §41):

```
Event written to CraftSQL
  → n CraftCOM agents selected (independent keys, different nodes)
  → Each: load WASM from CraftOBJ, validate event, call craft_sign()
  → Signatures broadcast via ATTEST_BROADCAST WireMessage
  → Coordinator collects k signatures, verifies each (~2.3µs), writes to CraftSQL
```

The `craft_sign()` host function returning 0 (stub) means no attestation works today. Fix `craft_sign` first.

---

## PART 10: PHASE 9 — NODE ASSEMBLY (craftec-node)

### craftec-node

**Current state:** All 9 subsystems initialize and start. `LoggingHandler` discards all inbound messages. Event bus loop only logs events. SWIM probes not dispatched. No SQL DB initialized. No piece announcements.

**What needs to be built:** Wire everything together.

#### Main Binary (main.rs — DONE)

```rust
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();
    let config = load_or_create_config()?;
    let node = CraftecNode::new(config).await?;
    node.run().await?;
    Ok(())
}
```

#### Startup Sequence (from spec §12 and §57)

The current 12-step sequence in `CraftecNode::new()` is correct. Add the missing steps:

```rust
pub async fn new(config: NodeConfig) -> Result<Self> {
    // Step 1: Create data directories
    tokio::fs::create_dir_all(&config.data_dir).await?;

    // Step 2: Write node.lock (warn if exists — unclean shutdown)
    let lock_path = config.data_dir.join(NODE_LOCK_FILENAME);
    if lock_path.exists() {
        tracing::warn!("node.lock exists — previous shutdown was unclean. Running crash recovery.");
        // TODO: run crash recovery (verify BLAKE3 on all stored CIDs)
    }
    tokio::fs::write(&lock_path, format!("{}", std::process::id())).await?;

    // Step 3: Load/generate Ed25519 keypair
    let keystore = KeyStore::new(&config.data_dir)?;
    tracing::info!("NodeID: {}", keystore.node_id());

    // Step 4: CraftOBJ store (16K cache items = 256 MB at 16 KB pages)
    let store = Arc::new(ContentAddressedStore::new(
        &config.data_dir.join("obj"),
        16_384,  // spec §27: 256 MB default LRU cache
    )?);

    // Step 5: RLNC engine
    let rlnc = Arc::new(RlncEngine::new());

    // Step 6: CID-VFS
    let vfs = Arc::new(CidVfs::new(store.clone(), config.page_size)?);

    // Step 7: Event bus
    let event_bus = Arc::new(EventBus::new(EVENT_BUS_CAPACITY));

    // Step 8: WASM runtime (slow — defer if using feature flag)
    let com_runtime = Arc::new(ComRuntime::new(DEFAULT_FUEL_LIMIT)?);

    // Step 9: iroh Endpoint (starts QUIC listener)
    let keypair = NodeKeypair::from_signing_key(keystore.signing_key().clone());
    let endpoint = Arc::new(CraftecEndpoint::new(&config, &keypair).await?);
    let swim = endpoint.swim().clone();

    // Step 10: PieceTracker + HealthScanner
    let piece_tracker = Arc::new(PieceTracker::new());
    let health_scanner = Arc::new(HealthScanner::new(
        store.clone(),
        piece_tracker.clone(),
        Duration::from_secs(config.health_scan_interval_secs / 100),  // interval per cycle
    ));

    // Step 11: Program scheduler
    let scheduler = Arc::new(ProgramScheduler::new(com_runtime.clone()));

    // Step 12: Shutdown channel
    let (shutdown_tx, _) = broadcast::channel::<()>(SHUTDOWN_CAPACITY);

    // MISSING: Initialize node's own CraftSQL database
    // TODO: CraftDatabase::create(keystore.node_id(), vfs.clone()).await?

    Ok(Self { config, keystore, store, rlnc, vfs, endpoint, swim,
               health_scanner, piece_tracker, com_runtime, scheduler,
               event_bus, shutdown_tx })
}
```

#### Run Loop — What Needs Wiring

```rust
pub async fn run(&self) -> Result<()> {
    // Bootstrap (connect to peers)
    if !self.config.bootstrap_peers.is_empty() {
        self.endpoint.bootstrap(&self.config.bootstrap_peers).await
            .unwrap_or_else(|e| tracing::warn!("Bootstrap failed: {}", e));
    }

    let (repair_tx, repair_rx) = mpsc::channel::<RepairRequest>(128);
    let pending_fetches = Arc::new(PendingFetches::new());

    // 1. Inbound message handler (REPLACE LoggingHandler with real dispatch)
    let handler = Arc::new(NodeMessageHandler {
        store: self.store.clone(),
        piece_tracker: self.piece_tracker.clone(),
        dht: Arc::new(DhtProviders::new()),
        rpc_handler: /* create RpcWriteHandler */,
        pending_fetches: pending_fetches.clone(),
    });
    let ep = self.endpoint.clone();
    tokio::spawn(async move { ep.accept_loop(handler).await });

    // 2. SWIM loop WITH probe dispatch
    let swim = self.swim.clone();
    let ep = self.endpoint.clone();
    let shutdown_rx = self.shutdown_tx.subscribe();
    tokio::spawn(async move { run_swim_loop(swim, ep, shutdown_rx).await });

    // 3. Health scanner WITH repair forwarding
    let scanner = self.health_scanner.clone();
    let shutdown_rx = self.shutdown_tx.subscribe();
    tokio::spawn(async move { scanner.run(repair_tx, shutdown_rx).await });

    // 4. Repair executor
    let repair_exec = RepairExecutor::new(self.rlnc.clone(), self.endpoint.clone(), self.piece_tracker.clone());
    let shutdown_rx = self.shutdown_tx.subscribe();
    tokio::spawn(async move {
        let mut rx = repair_rx;
        loop {
            tokio::select! {
                Some(req) = rx.recv() => { let _ = repair_exec.execute_repair(&req).await; }
                _ = shutdown_rx.recv() => break,
            }
        }
    });

    // 5. Event bus routing (REPLACE logging-only with real routing)
    let mut event_rx = self.event_bus.subscribe();
    let store = self.store.clone();
    let ep = self.endpoint.clone();
    tokio::spawn(async move {
        while let Ok(event) = event_rx.recv().await {
            match event {
                Event::CidWritten { cid } => {
                    // Announce to DHT + peers
                    ep.announce_cid_to_peers(&cid).await;
                }
                Event::PeerDisconnected { node_id } => {
                    piece_tracker.remove_node(&node_id);
                }
                Event::DiskWatermarkHit { usage_percent } => {
                    tracing::warn!("Disk at {}% — triggering eviction", usage_percent);
                    // Trigger eviction agent
                }
                Event::ShutdownSignal => break,
                _ => {}
            }
        }
    });

    // Wait for shutdown signal
    wait_for_shutdown().await;

    // Graceful shutdown sequence (spec §14)
    tracing::info!("Shutting down...");
    let _ = self.event_bus.publish(Event::ShutdownSignal);
    let _ = self.shutdown_tx.send(());

    // Allow active writes to complete (30s timeout)
    tokio::time::sleep(Duration::from_secs(5)).await;

    // Remove sentinel file (LAST write — confirms clean shutdown)
    let lock_path = self.config.data_dir.join(NODE_LOCK_FILENAME);
    let _ = tokio::fs::remove_file(&lock_path).await;

    Ok(())
}
```

#### Real Message Handler

```rust
pub struct NodeMessageHandler {
    store: Arc<ContentAddressedStore>,
    piece_tracker: Arc<PieceTracker>,
    dht: Arc<DhtProviders>,
    rpc_handler: Arc<RpcWriteHandler>,
    pending_fetches: Arc<PendingFetches>,
}

impl ConnectionHandler for NodeMessageHandler {
    fn handle_message(&self, from: NodeId, msg: WireMessage) -> HandlerFuture {
        let store = self.store.clone();
        let tracker = self.piece_tracker.clone();
        let dht = self.dht.clone();
        let rpc = self.rpc_handler.clone();
        let pending = self.pending_fetches.clone();

        Box::pin(async move {
            match msg {
                WireMessage::Ping { nonce } => {
                    Some(WireMessage::Pong { nonce })
                }
                WireMessage::PieceRequest { cid, piece_indices: _ } => {
                    // Serve coded pieces from local store
                    // TODO: look up stored piece CIDs for this content CID
                    None
                }
                WireMessage::PieceResponse { pieces } => {
                    // Unblock pending fetches waiting for these pieces
                    for piece in pieces {
                        pending.resolve(&piece.cid, piece);
                    }
                    None
                }
                WireMessage::ProviderAnnounce { cid, node_id } => {
                    dht.announce_provider(&cid, &node_id);
                    None
                }
                WireMessage::HealthReport { cid, available_pieces, target_pieces: _ } => {
                    // Record that this node holds a coded piece for this CID.
                    // RLNC: no piece_index — every piece is unique.
                    tracker.record_piece(&cid, PieceHolder {
                        node_id: from,
                        last_seen: Instant::now(),
                    });
                    None
                }
                WireMessage::SignedWrite { payload, signature, writer, cas_version: _ } => {
                    // Parse SignedWrite payload, forward to RpcWriteHandler
                    // TODO: deserialize payload → SignedWrite struct
                    None
                }
                _ => None,
            }
        })
    }
}
```

#### Configuration Loading

```rust
fn load_or_create_config() -> anyhow::Result<NodeConfig> {
    let path = std::path::Path::new("craftec.json");
    if path.exists() {
        NodeConfig::load(path).map_err(|e| anyhow::anyhow!("Config load failed: {}", e))
    } else {
        let config = NodeConfig::default();
        config.save(path).map_err(|e| anyhow::anyhow!("Config save failed: {}", e))?;
        tracing::info!("Created default config at {}", path.display());
        Ok(config)
    }
}
```

Add CLI argument parsing (using `clap` or manual `std::env::args`) for:
- `--config <path>` — override config file location
- `--addpeer <node_id>` — bootstrap peer
- `--data-dir <path>` — override data directory
- `--log-level <level>` — override log level

#### Crash Recovery (spec §15)

On detecting stale `node.lock`:
```rust
async fn run_crash_recovery(store: &ContentAddressedStore) {
    tracing::warn!("Running crash recovery...");
    let cids = store.list_cids().await.unwrap_or_default();
    let mut corrupt_count = 0;
    for cid in &cids {
        if let Ok(Some(data)) = store.get(cid).await {
            if !cid.verify(&data) {
                tracing::error!("Corrupt CID detected: {}. Deleting.", cid);
                let _ = store.delete(cid).await;
                corrupt_count += 1;
            }
        }
    }
    tracing::info!("Crash recovery complete. Removed {} corrupt CIDs.", corrupt_count);
    // Corrupt CIDs will be re-fetched from peers via HealthScan
}
```

---

## PART 11: INTEGRATION & END-TO-END FLOWS

### Flow 1: WRITE PATH — User stores data

**Crates involved:** craftec-rlnc, craftec-obj, craftec-net

```
User calls store(data):

1. [craftec-rlnc] RlncEngine::encode(data, k=32)
   → 80 coded pieces (for k=32, redundancy=2.5x)
   → Each piece: {cid=BLAKE3(data), coding_vector=[32 bytes], data=[piece_size bytes], hommac_tag}

2. [craftec-obj] ContentAddressedStore::put(original_data)
   → data CID = BLAKE3(data)
   → Write to data/obj/<shard>/<cid-hex>
   → Update bloom filter + LRU cache
   → Emit Event::CidWritten { cid }

3. [craftec-net] Event::CidWritten triggers announce_cid_to_peers(cid)
   → ProviderAnnounce message gossipped to log(N) alive peers
   → DhtProviders::announce_provider(cid, our_node_id)

4. [craftec-net] Distribute coded pieces to peers:
   → Each coded piece is unique (random GF(2^8) coefficient vector) — no rarest-first
   → Send each piece to a distinct peer that doesn't yet hold a piece for this CID
   → Distribute n=2k+16 pieces across ≥K distinct peers
   → Targets: store pieces in their ContentAddressedStore
   → Targets: announce to DHT

5. [craftec-health] Pieces tracked in PieceTracker via ProviderAnnounce/HealthReport
```

**Data crossing crate boundaries:**
- `craftec-rlnc` → `craftec-obj`: `Vec<CodedPiece>`
- `craftec-obj` → event bus: `Event::CidWritten { cid }`
- `craftec-node` → `craftec-net`: `WireMessage::PieceResponse { pieces }`

**Failure handling:**
- RLNC encode fails: `CodingError` — data not stored
- Store put fails (disk full): `ObjError::StoreFull` — trigger eviction then retry once
- Distribution fails: pieces not sent — HealthScan detects under-replication on next cycle

### Flow 2: READ PATH — User fetches content by CID

**Crates involved:** craftec-obj, craftec-net, craftec-rlnc

```
User calls fetch(cid):

1. [craftec-obj] ContentAddressedStore::get(cid)
   → Check LRU cache (50ns if hit)
   → Check bloom filter (fast negative)
   → Check disk (10µs)
   → If found locally: return immediately (no network)

2. If not found locally:
   [craftec-net] DhtProviders::get_providers(cid) → list of NodeIds
   → Filter out SWIM-dead nodes (local dead set)

3. [craftec-net] Send WireMessage::PieceRequest { cid } to top providers
   → Parallel fetch from multiple providers (at least k requests)
   → timeout: 10s per piece

4. [craftec-net] Receive WireMessage::PieceResponse { pieces }
   → PendingFetches::resolve(cid, piece) unblocks waiting decoders

5. [craftec-rlnc] RlncDecoder:
   → add_piece() for each received piece
   → is_decodable() when rank == k
   → decode() → original data

6. Verify BLAKE3(decoded_data) == cid (critical — detect corrupt pieces)
   → If mismatch: ban_score++ on the offending peer, re-fetch from another provider
```

**Functions called:**
- `ContentAddressedStore::get(cid)` → `Option<Bytes>`
- `DhtProviders::get_providers(cid)` → `Vec<NodeId>`
- `CraftecEndpoint::send_message(peer, WireMessage::PieceRequest)` → `Result<()>`
- `RlncDecoder::add_piece(piece)` → `Result<bool>`
- `RlncDecoder::decode()` → `Result<Vec<u8>>`

**What can fail:**
- DHT returns no providers: content not yet announced — wait and retry
- Fewer than k pieces arrive before timeout: `InsufficientPieces` → return SQLITE_IOERR_READ
- BLAKE3 mismatch: corrupt piece — ban peer, re-fetch

### Flow 3: SQL WRITE — RPC write path

**Crates involved:** craftec-sql, craftec-vfs, craftec-obj, craftec-net

```
Client sends SIGNED_WRITE to any node:

1. [craftec-net] Receive WireMessage::SignedWrite
   → Route to RpcWriteHandler

2. [craftec-sql] RpcWriteHandler::handle_signed_write(msg):
   a. verify_signature(writer_pubkey, payload, signature) → InvalidSignature
   b. check_ownership(writer, owner) → UnauthorizedWriter
   c. check_cas(expected_root, current_root) → CasConflict (client retries)
   d. database.execute(sql, writer) → triggers libsql → VFS

3. [craftec-vfs] CidVfs:
   a. SQLite sends xWrite(page_num, data) → dirty_pages[page_num] = data
   b. SQLite sends xSync() → CidVfs.commit():
      - BLAKE3 each dirty page → CID
      - store.put(page_data) for each dirty page
      - Serialize updated page_index
      - store.put(index_bytes) → index_cid
      - Atomically: current_root = index_cid

4. [craftec-obj] ContentAddressedStore::put(page_data) for each dirty page

5. Return new_root_cid to client via WireMessage::WriteResult

6. Async (off critical path):
   - Emit Event::PageCommitted { db_id, page_num, root_cid }
   - Event bus triggers: RLNC encode each new page CID
   - Distribute coded page pieces to peers
   - Announce new root CID via DHT + Pkarr + SWIM gossip
```

**CAS conflict handling:**
```
Client:
  1. GET current root CID via DHT (or Pkarr: "db.{owner_node_id}.craftec")
  2. Sign: SIGNED_WRITE(sql, expected_root=current_root, sig=Ed25519(payload))
  3. Send to any node
  4. If response = CasConflict: goto 1 (retry with fresh root)
  5. If response = new_root_cid: success
```

### Flow 4: SQL READ — Distributed page fetch

```
User issues SQL query:

1. [craftec-sql] database.query(sql):
   - snapshot = vfs.snapshot() → pins current root CID

2. [craftec-vfs] CidVfs::read_page(page_num) for each SQLite page access:
   a. page_cache.get(root_cid, page_num) → LRU hit (~50ns)
   b. page_index.get(page_num) → CID
   c. store.get(cid) → local disk read (~10µs)
   d. If not in local store: network fetch (Flow 2 above)

3. SQLite executes query over pages, returns rows

4. [craftec-sql] Materialize rows into Vec<Row>

5. drop(snapshot) → release root CID pin
```

**Offline behavior:** If k or more piece-holders are unreachable, `read_page()` returns `VfsError::StoreError` → SQLite gets `SQLITE_IOERR_READ` → query fails with `CRAFTEC_CONTENT_UNAVAILABLE`. The application should surface this as "content offline" (spec §33).

### Flow 5: REPAIR PATH — Health scan detects under-replication

```
Trigger: HealthScanner cycle (every 5 minutes) or PeerDisconnected event

1. [craftec-health] HealthScanner::scan_cycle():
   - Take 1% of PieceTracker.sorted_cids()
   - Only evaluate CIDs where THIS node holds ≥2 coded pieces (recode eligibility)
   - For each eligible CID: query DHT for total coded piece count across network
   - Compare count vs target n (= 2k + 16)
   - Emit RepairRequest::Normal or RepairRequest::Critical

2. [craftec-health] RepairRequest forwarded via mpsc channel to RepairExecutor

3. [craftec-health] RepairExecutor::execute_repair():
   - Compute deficit = target - current_count
   - Rank all holders with ≥2 pieces by (uptime DESC, reputation DESC, NodeId ASC)
   - Am I in the top N (where N = deficit)? If NOT: skip (other nodes handle it)
   - Top-N determination is deterministic — all nodes compute the same ranking

4. [craftec-health] Recode from LOCAL pieces (no network fetch):
   - Load ≥2 coded pieces from this node's ContentAddressedStore
   - RlncEngine::recode(local_pieces): fresh random GF(2^8) coefficients
   - new_cv = Σ αᵢ * pieces[i].cv
   - new_data = Σ αᵢ * pieces[i].data
   - NO DECODE — O(piece_size) not O(k * piece_size)

5. [craftec-net] Select distribution target:
   Priority 1: peers with 1 piece (bring to ≥2 so they can join future repair)
   Priority 2: peers with 0 pieces (non-holders)
   Send WireMessage::PieceResponse { pieces: [recoded_piece] }

6. Target stores recoded piece, announces to DHT
   PieceTracker updated with new holder
```

**Rate limiting (spec §30):** 1 piece per CID per cycle **per elected node**. If deficit = 10, the top 10 nodes each produce 1 piece = 10 pieces repaired per cycle. Prevents repair storms while still allowing parallel recovery during large-scale failures.

### Flow 6: JOIN PATH — New node bootstraps

```
1. Binary starts. init.done check.
   - If missing: generate Ed25519 keypair, create directories, init schema
   - NodeId = Ed25519 public key (iroh convention)

2. Write node.lock. If exists: crash recovery first.

3. Load config. Raise FD limit: setrlimit(RLIMIT_NOFILE, 65535)

4. Initialize subsystems in order:
   CraftOBJ → CidVfs → RLNC → ComRuntime → EventBus → iroh Endpoint → SWIM

5. Bootstrap:
   a. iroh relay server: immediate QUIC connectivity (no bootstrap needed)
   b. DNS seeds (6-8 hostnames) → resolve → attempt 8 in parallel
   c. --addpeer CLI flags → highest priority
   d. Hardcoded IPs as last resort
   → Wait for ≥ 3 connections

6. Per connection: TLS 1.3 handshake (inside QUIC), ALPN selection, HELLO exchange
   [craftec-net] Send WireMessage::SwimJoin { node_id, listen_port }
   [craftec-net] Receive SwimJoin from peers → SwimMembership::mark_alive()

7. Admission checks (spec §20):
   - Verify NodeId == Ed25519 public key
   - Max 2 nodes from same /24 subnet
   - Check ban list

8. Storage bootstrap:
   - Check CraftOBJ: which CIDs held locally
   - For each: announce to DHT via ProviderAnnounce
   - PieceTracker: record self as holder for each stored piece

9. SWIM gossip begins: ProbeTargets from protocol_tick() dispatched over network

10. Full participation: accepting connections, serving pieces, running health scans
```

---

## PART 12: TESTING STRATEGY

### Unit Test Requirements Per Crate

| Crate | Current Tests | Missing Tests (Priority) |
|---|---|---|
| craftec-types | ~30 | Wire frame header encoding; NodeId-iroh compatibility |
| craftec-crypto | ~20 | HomMAC homomorphic property; GF polynomial consistency |
| craftec-rlnc | ~57 | Decode with only recoded pieces; SIMD parity check |
| craftec-obj | ~53 | Concurrent puts same CID; bloom rebuild after restart |
| craftec-vfs | ~22 | xRead zero-fill for unallocated pages; rebuild from disk |
| craftec-sql | ~20 | Real SQL INSERT → query roundtrip (requires libsql) |
| craftec-net | ~31 | SWIM indirect probe; connection score-based eviction |
| craftec-health | ~30 | Repair executor with real pieces; scanner→executor wiring |
| craftec-com | ~14 | craft_store_get with real store; fuel exhaustion behavior |
| craftec-node | minimal | Node startup/shutdown cycle; inbound message routing |

### What multi_node.rs Covers vs. What's Missing

**Covers (10 tests):**
- SWIM discovery (in-process, no real network)
- RLNC encode → distribute → decode (in-process HashMap)
- RLNC recode at intermediate node
- Wire message round-trips for all variants
- Ed25519 cross-node verification
- HomMAC cross-node integrity
- Full pipeline: encode → sign → transmit → verify → decode (in-process)
- k=32 large data distribution

**Missing:**
- Real iroh QUIC connection between two processes
- Piece exchange over actual network
- CraftSQL write → query over network (requires libsql fix)
- Health scan detecting repair need and executing repair
- SWIM failure detection (marking a node dead)
- RPC write conflict (CAS conflict resolution)
- DHT provider record propagation

### How to Test Networking Locally

```bash
# Terminal 1: Start node A
RUST_LOG=debug ./target/debug/craftec --data-dir /tmp/node-a --config /tmp/node-a/config.json

# Terminal 2: Start node B with A as bootstrap
RUST_LOG=debug ./target/debug/craftec --data-dir /tmp/node-b \
    --bootstrap-peer $(cat /tmp/node-a/data/node.pub | xxd -p | tr -d '\n')
```

Or use the test helper (to be written):
```rust
// tests/helpers.rs
pub async fn spawn_local_node(port: u16, data_dir: &Path) -> CraftecNode {
    let mut config = NodeConfig::default();
    config.listen_port = port;
    config.data_dir = data_dir.to_path_buf();
    CraftecNode::new(config).await.expect("node start")
}

#[tokio::test]
async fn two_nodes_connect() {
    let dir_a = tempdir().unwrap();
    let dir_b = tempdir().unwrap();
    let node_a = spawn_local_node(14430, dir_a.path()).await;
    let node_id_a = node_a.keystore().node_id();

    let mut config_b = NodeConfig::default();
    config_b.bootstrap_peers = vec![node_id_a.to_string()];
    let node_b = spawn_local_node(14431, dir_b.path()).await;

    // Give SWIM time to converge
    tokio::time::sleep(Duration::from_secs(2)).await;

    assert!(node_b.swim().is_alive(&node_id_a));
    assert!(node_a.swim().is_alive(&node_b.keystore().node_id()));
}
```

### How to Test RLNC End-to-End

```rust
#[tokio::test]
async fn rlnc_encode_distribute_decode_e2e() {
    let data = b"Hello, distributed world!".repeat(1000);
    let engine = RlncEngine::new();
    let k = 8;

    // Encode
    let pieces = engine.encode(data, k).await.unwrap();
    let cid = pieces[0].cid;
    let piece_size = pieces[0].data.len();
    assert_eq!(pieces.len() as u32, target_n(k));

    // Simulate distribution: 3 storage nodes
    let mut node_stores: [Vec<CodedPiece>; 3] = Default::default();
    for (i, piece) in pieces.into_iter().enumerate() {
        node_stores[i % 3].push(piece);
    }

    // Collect pieces from "network" (round-robin from nodes)
    let mut decoder = RlncDecoder::new(k, piece_size);
    let mut node_idx = 0;
    while !decoder.is_decodable() {
        let node = &node_stores[node_idx % 3];
        if let Some(piece) = node.get(node_idx / 3) {
            decoder.add_piece(piece).unwrap();
        }
        node_idx += 1;
        assert!(node_idx < 1000, "took too many pieces");
    }

    let decoded = decoder.decode().unwrap();
    assert_eq!(&decoded[..data.len()], data.as_slice());

    // Verify CID
    assert_eq!(cid, Cid::from_data(data));
}
```

### How to Test CraftSQL Over the Network

After implementing libsql integration:

```rust
#[tokio::test]
async fn sql_write_and_read() {
    let dir = tempdir().unwrap();
    let store = Arc::new(ContentAddressedStore::new(&dir.path().join("obj"), 1024).unwrap());
    let vfs = Arc::new(CidVfs::with_default_page_size(store.clone()).unwrap());

    let keypair = NodeKeypair::generate();
    let owner = keypair.node_id();

    let db = CraftDatabase::create(owner, vfs.clone()).await.unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, val TEXT)", &owner).await.unwrap();
    db.execute("INSERT INTO t VALUES (1, 'hello')", &owner).await.unwrap();

    let rows = db.query("SELECT val FROM t WHERE id = 1").await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], ColumnValue::Text("hello".to_string()));
}

#[tokio::test]
async fn sql_rpc_write_cas_conflict() {
    // Two concurrent SIGNED_WRITEs to same database
    // First one succeeds; second gets CasConflict; second retries and succeeds
    todo!()
}
```

### Performance / Scale Testing

**3-node scenario:** Basic functionality. All unit + integration tests should pass.

**10-node scenario:**
- SWIM converges within 5 rounds (~5s for 10 nodes at 500ms intervals)
- DHT provider records propagate within log(10) ≈ 4 hops
- Health scan covers all CIDs within 8 hours
- Write throughput: limited by RLNC encode (single node) and QUIC bandwidth

**100-node scenario:**
- SWIM converges within ~7 rounds (~3.5s)
- Test CAS conflict rate under concurrent writes (should be low for single-writer-per-identity)
- Test piece distribution: verify no node holds >2 pieces of same CID initially
- Test repair storm prevention: kill 10 nodes simultaneously, verify repair is gradual

**1M-node scenario (theoretical):**
- SWIM convergence: O(log 1M) ≈ 20 rounds ≈ 10s
- DHT lookup: 1–3 hops × 50ms = 50–150ms (spec §18)
- HealthScan: each node scans only its own CIDs at 1%/cycle — aggregate load distributed
- Connection pool: max 200 connections per node; 1M nodes × 200 = no problem (sparse graph)

---

## PART 13: KNOWN ISSUES & PITFALLS

### Compilation: wasmtime/cranelift Build Time

First compile of `wasmtime 29` with cranelift takes **10–45 minutes** depending on hardware. cranelift is the JIT compiler backend — it's large.

**Mitigation:** Feature-gate wasmtime during development:

```toml
# craftec-com/Cargo.toml
[features]
default = []
wasm-runtime = ["wasmtime"]

[dependencies]
wasmtime = { workspace = true, optional = true }
```

```rust
// craftec-com/src/runtime.rs
#[cfg(feature = "wasm-runtime")]
pub struct ComRuntime { /* real implementation */ }

#[cfg(not(feature = "wasm-runtime"))]
pub struct ComRuntime { fuel_limit: u64 }  // stub for dev builds
```

For CI and release builds, always compile with `--features wasm-runtime`.

### ed25519-dalek v2 API: `from_bytes` Takes Reference

```rust
// WRONG (compiles only with v1):
let signing_key = SigningKey::from_bytes(secret_bytes);

// CORRECT (v2):
let signing_key = SigningKey::from_bytes(&secret_bytes);
```

The current `NodeKeypair::from_secret_bytes(bytes: &[u8; 32])` already uses the correct reference API. Watch for this in any new code that constructs `SigningKey` directly.

Also: `VerifyingKey::from_bytes()` changed signature in v2. Use `VerifyingKey::try_from(&bytes[..])` for byte slice input.

### Rust 2024 Edition: `gen` is a Reserved Keyword

`gen` is a reserved keyword in Rust 2024 for generator syntax. `rand::Rng::gen()` conflicts.

```rust
// BREAKS in Rust 2024:
use rand::Rng;
let x: u8 = rng.gen();

// WORKS in Rust 2024:
use rand::Rng;
let x: u8 = rng.r#gen();    // raw identifier
// OR:
let x: u8 = rng.random();   // rand 0.9+ alternative
// OR:
let x: u8 = rng.gen_range(0..=255);
```

The workspace uses `rand = "0.8"` which predates the Rust 2024 edition restriction. The method still exists but triggers reserved keyword warnings. Use `r#gen()` or `gen_range(0..=u8::MAX)` in all new code.

### iroh 0.96: API Changes from Earlier Versions

iroh 0.32+ merged `iroh-net` into `iroh`. The main changes:

| Old (0.32) | New (0.96) |
|---|---|
| `iroh_net::Endpoint` | `iroh::Endpoint` |
| `iroh::node::Node` | `iroh::Endpoint` (simpler) |
| `Endpoint::builder().bind(0).await` | `Endpoint::builder().bind().await` |
| `conn.open_bi()` | Same |
| `iroh::NodeId` | `iroh::PublicKey` |

The current code uses `iroh::Endpoint` directly (correct for 0.96). If upgrading from older iroh, watch for these renames.

**iroh::PublicKey vs craftec_types::NodeId:** iroh uses `iroh::PublicKey` (32 bytes, Ed25519) for its node identity. Our `NodeId` is also 32 bytes, same Ed25519 key. They are the **same bytes** — conversion:

```rust
// craftec NodeId → iroh PublicKey
let iroh_pk = iroh::PublicKey::from_bytes(our_node_id.as_bytes())?;

// iroh PublicKey → craftec NodeId
let our_node_id = NodeId::from_bytes(*iroh_pk.as_bytes());
```

### Cargo.lock: Commit It

`Cargo.lock` must be committed for the binary crate (`craftec-node`). This ensures reproducible builds. If using CI, pin the exact Rust version (1.85) and commit `Cargo.lock`.

### Feature Flags for Faster Dev Builds

```toml
# workspace Cargo.toml — add dev feature
[features]
dev = ["craftec-com/stub-wasm"]  # skip wasmtime compilation
```

Use `cargo check --workspace --features dev` for fast iteration cycles.

### Memory: Wasmtime Default Heap

Wasmtime's default memory is 4 GiB address space per instance (using virtual memory, actual physical use is per-access). The spec limit of 256 MB applies to actual memory use. Enforce with:

```rust
let mut config = Config::new();
config.static_memory_maximum(256 * 1024 * 1024);  // 256 MB static memory limit
```

Or catch OOM errors from the guest (they surface as Trap errors).

### GF(2^8) Polynomial Inconsistency

`gf256.rs` uses AES polynomial `0x11B`; `hommac.rs` uses `0x11D` (wrong). Standardize on `0x11B`. Verify consistency:

```rust
// These must agree:
assert_eq!(craftec_rlnc::gf256::gf_mul(0x53, 0xCA), 1);     // gf256.rs
assert_eq!(craftec_crypto::hommac::gf256_mul(0x53, 0xCA), 1); // hommac.rs
```

### DashMap 6 Compatibility

dashmap 6 changed the API for `entry()`. Use:
```rust
// dashmap 6:
map.entry(key).or_default().push(value);
// NOT:
map.entry(key).or_insert_with(Vec::new).push(value);
```

### thiserror 2 Changes

thiserror 2 is largely compatible with v1. Main change: `#[error(transparent)]` for wrapping no longer requires the `source` attribute separately. Existing code should compile without changes.

---

## PART 14: IMPLEMENTATION PRIORITY & TIMELINE

### Ordered by What Unblocks the Most

#### Priority 1: Fix All Compilation Errors (Prerequisite, 1–2 days)

**What:** Ensure `cargo build --workspace` succeeds with zero errors.

**Known issues:**
- Any Rust 2024 `gen` keyword conflicts in random number calls
- ed25519-dalek v2 API mismatches if any remain
- Feature flag configuration for wasmtime (if compile time is a blocker)

**Acceptance criteria:** `cargo build --workspace` exits 0. `cargo test --workspace` exits 0 (all 287 tests pass).

---

#### Priority 2: craftec-net — Get Two Nodes Talking (2–3 days)

**What:** Fix SWIM probe dispatch. Wire up inbound message handler. Verify two nodes can connect and see each other in SWIM.

**Effort:** Medium — iroh endpoint is scaffolded, just needs dispatch wiring.

**Dependencies:** Priority 1.

**Implementation steps:**
1. Update `run_swim_loop` signature to accept `Arc<CraftecEndpoint>`
2. Dispatch `protocol_tick()` probes via `endpoint.send_message()`
3. Fix `handle_swim_conn` to send handle_message() responses back
4. Write `two_nodes_connect` integration test

**Acceptance criteria:** Two local nodes start, SWIM loop runs, both nodes appear as Alive in each other's `swim.alive_members()` within 5 seconds.

---

#### Priority 3: craftec-net — Piece Exchange Over QUIC (3–5 days)

**What:** Implement real message routing in `NodeMessageHandler`. Implement `PendingFetches` rendezvous for piece responses.

**Dependencies:** Priority 2.

**Implementation steps:**
1. Implement `PendingFetches` struct in craftec-node
2. Replace `NullHandler`/`LoggingHandler` with `NodeMessageHandler`
3. Wire `PieceRequest` → serve from `ContentAddressedStore`
4. Wire `PieceResponse` → `PendingFetches::resolve()`
5. Wire `ProviderAnnounce` → `DhtProviders::announce_provider()`
6. Write `piece_exchange_two_nodes` integration test

**Acceptance criteria:** Node A stores data, Node B fetches it by CID over real QUIC connection. BLAKE3 verified at receiver.

---

#### Priority 4: craftec-health — Real Health Scanning + Repair (3–5 days)

**What:** Implement parallel repair via Natural Selection. Nodes with ≥2 local pieces recode and distribute — no network fetch for recode input. Wire event bus to populate PieceTracker.

**Dependencies:** Priority 3 (needs real piece distribution on write).

**Implementation steps:**
1. Fix `target_piece_count()`: target = 2k + 16 (not just the redundancy factor)
2. Add scan eligibility filter: only evaluate CIDs where this node holds ≥2 pieces
3. Implement `elected_repairers()`: rank holders, compute top-N from deficit
4. Implement local recode: load pieces from own ContentAddressedStore, recode with fresh coefficients
5. Implement distribution priority: 1-piece holders first, then non-holders
6. Wire `PeerDisconnected` event → `PieceTracker::remove_node()`
7. Wire `ProviderAnnounce` inbound → `PieceTracker::record_piece()`
8. Write `repair_under_replicated_cid` integration test (5+ nodes, kill 1, verify parallel repair)

**Acceptance criteria:** Kill one of five nodes holding pieces. Multiple surviving nodes (those with ≥2 pieces and in top-N ranking) independently detect under-replication and each produce 1 new coded piece from local recode. Pieces distributed to 1-piece holders first, then non-holders. PieceTracker shows target replication restored.

---

#### Priority 5: craftec-sql — libsql Integration (5–7 days)

**What:** Implement SQLite VFS plugin using CidVfs. Wire libsql to CidVfs. Fix `execute()` and `query()`.

**Dependencies:** Priority 1 (compile). craftec-vfs is already done.

**Implementation steps:**
1. Add `libsql` to `craftec-sql/Cargo.toml`
2. Implement `CraftecVfs` struct (xOpen, xDelete, xAccess)
3. Implement `CraftecVfsFile` struct (xRead, xWrite, xSync, xLock, xClose)
4. Register VFS with libsql `Builder`
5. Set `PRAGMA page_size = 16384` and `PRAGMA journal_mode = DELETE`
6. Update `CraftDatabase::execute()` to use `conn.execute()`
7. Update `CraftDatabase::query()` to use `conn.query()` and materialize rows
8. Write SQL roundtrip tests (CREATE TABLE → INSERT → SELECT)

**Acceptance criteria:** SQL INSERT followed by SELECT returns correct rows. Root CID changes after each write. Two snapshots at different root CIDs return different data.

---

#### Priority 6: craftec-com — Wasmtime Host Functions (4–6 days)

**What:** Implement `craft_store_get`, `craft_store_put`, `craft_sql_query`, `craft_sign` with real backing stores.

**Dependencies:** Priority 5 (sql works).

**Implementation steps:**
1. Change `Linker<()>` to `Linker<HostState>` where `HostState` holds `Arc<ContentAddressedStore>` + `Arc<CraftDatabase>` + `Arc<KeyStore>`
2. Implement `craft_store_get`: read CID from WASM memory, call `store.get()`, write result to scratch buffer
3. Implement `craft_store_put`: read data from WASM memory, call `store.put()`, return CID bytes
4. Implement `craft_sql_query`: read SQL from WASM memory, call `database.query()`, serialize rows
5. Implement `craft_sign`: read message from WASM memory, call `keystore.sign()`, write signature
6. Implement rate limiting per spec §40
7. Implement `ProgramScheduler::start_program()` to actually spawn background tasks

**Acceptance criteria:** A test WASM module that calls `craft_store_put(data)` → `craft_store_get(returned_cid)` receives the original data. `craft_sign(hash)` returns a valid 64-byte signature.

---

#### Priority 7: craftec-node — Wire Everything Together (5–7 days)

**What:** Event bus routing to subsystems. SQL DB initialization on startup. Agent auto-start. SWIM probe dispatch fixes. DHT gossip propagation.

**Dependencies:** Priorities 2–6.

**Implementation steps:**
1. Initialize `CraftDatabase` on node startup
2. Wire event bus: `CidWritten` → DHT announce; `PeerDisconnected` → tracker cleanup; `DiskWatermarkHit` → eviction trigger
3. Auto-start built-in agents (LocalEviction, ReputationScoring) on boot
4. Add `PendingFetches` to node state, wire into message handler and repair executor
5. Implement DHT gossip: `announce_cid_to_peers()` using epidemic gossip
6. Wire `announce_cid_to_peers` into `CidWritten` event handler

**Acceptance criteria:** Node starts, initializes its own CraftSQL database, begins SWIM gossip, health scanner runs, event bus routes events to correct subsystems.

---

#### Priority 8: Integration Tests with Real Network (5–7 days)

**What:** Write `craftec-tests` integration tests that use real QUIC connections between multiple `CraftecNode` instances.

**Dependencies:** Priority 7.

**Tests to write:**
```rust
// 1. Two nodes connect via QUIC
// 2. Three nodes: write on A, read on C (via B)
// 3. Five nodes: SWIM discovers all members
// 4. SQL write on node A, query on node B (requires root CID publication + page fetching)
// 5. Kill one of five nodes: HealthScan detects repair need, repair succeeds
// 6. RPC write: client signs write, sends to node A, A executes and returns new root CID
// 7. CAS conflict: two concurrent RPC writes, first wins, second retries
```

---

#### Priority 9: Multi-Node Test Scenarios (3–5 days per scenario)

**3-node scenario:**
- All basic flows work
- Write on node 1, read on node 3
- Kill node 2, verify data still accessible from nodes 1 and 3

**10-node scenario:**
- SWIM convergence time measurement
- Write throughput under concurrent writers (same-identity: no conflict; different-identity: independent)
- Churn test: 2 nodes join and leave per minute

**100-node scenario:**
- Piece distribution efficiency: verify ~2 pieces/node after initial distribution
- Repair storm prevention: simultaneous failure of 10 nodes
- Memory usage per node under load

### Summary Table

| Priority | Task | Effort | Unblocks |
|---|---|---|---|
| 1 | Fix compilation | 1-2 days | Everything |
| 2 | Net: two nodes talking | 2-3 days | Piece exchange, health |
| 3 | Net: piece exchange | 3-5 days | Health repair, SQL net reads |
| 4 | Health: real repair | 3-5 days | Network durability |
| 5 | SQL: libsql integration | 5-7 days | SQL over network, WASM |
| 6 | COM: host functions | 4-6 days | WASM agents, attestation |
| 7 | Node: wire together | 5-7 days | Full system |
| 8 | Integration tests | 5-7 days | Confidence for production |
| 9 | Multi-node scenarios | 3-5 days each | Scale validation |

**Critical path:** 1 → 2 → 3 → 4 runs in parallel with 1 → 5. The long pole is SQL (5), which takes the most new code (SQLite VFS implementation). Networking and health can be developed in parallel with SQL.

**HomMAC fix** (combine_tags in craftec-crypto) should be done alongside Priority 5, before any production deployment. It doesn't block MVP functionality but is required for the pollution attack defense to work correctly.

---

*Document generated from v3.3 Technical Foundation (93 pages) and complete codebase audit (2026-03-03). Section numbers in parentheses (e.g., §27) reference the v3.3 spec.*
