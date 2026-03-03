# Testing Guide

This document describes how to run, extend, and verify the Craftec test suite.

---

## Prerequisites

- **Rust 1.85** or later
  ```sh
  rustup update stable
  rustc --version   # should print rustc 1.85.x or newer
  ```
- **cargo** (included with Rust)
- On Linux/macOS: no additional system dependencies
- On Windows: Rust stable target `x86_64-pc-windows-msvc` or `x86_64-pc-windows-gnu`

---

## Running the Full Test Suite

```sh
# Run all tests across every crate in the workspace
cargo test --workspace

# Run with debug logs enabled (useful for diagnosing test failures)
RUST_LOG=debug cargo test --workspace

# Run with full backtraces
RUST_BACKTRACE=1 cargo test --workspace

# Combine both
RUST_LOG=trace RUST_BACKTRACE=full cargo test --workspace
```

---

## Running Tests for a Specific Crate

```sh
# Test a single crate by package name
cargo test -p craftec-types
cargo test -p craftec-crypto
cargo test -p craftec-obj
cargo test -p craftec-rlnc
cargo test -p craftec-vfs
cargo test -p craftec-sql
cargo test -p craftec-net
cargo test -p craftec-health
cargo test -p craftec-com
cargo test -p craftec-node

# Run a specific test function
cargo test -p craftec-rlnc encode_decode_roundtrip

# Run tests matching a pattern
cargo test -p craftec-rlnc -- gf256

# Show test output even for passing tests
cargo test -p craftec-rlnc -- --nocapture
```

---

## Test Categories

### Unit Tests

Unit tests live in `#[cfg(test)]` modules at the bottom of each source file.
They test individual functions and types in isolation with no network or disk I/O
(except where `tempfile::tempdir()` provides a throwaway directory).

```sh
# Run all unit tests
cargo test --workspace --lib
```

### Integration Tests

Integration tests live in `tests/` directories under each crate (none yet — planned).
They test cross-crate behaviour and multi-step workflows.

```sh
# Run only integration tests (once they exist)
cargo test --workspace --test '*'
```

---

## Per-Crate Test Guide

### craftec-types

```sh
cargo test -p craftec-types
```

Key test areas:
- `config` — `NodeConfig::default()` field values, JSON round-trip save/load, invalid JSON error path
- `identity` — `NodeKeypair::generate()`, sign/verify, secret byte round-trip, `NodeId` slice construction errors
- `event` — channel capacity constants are positive, `Event::clone()` preserves variant and fields

```sh
# Verify NodeConfig defaults
cargo test -p craftec-types default_values

# Verify config JSON round-trip
cargo test -p craftec-types save_and_load_round_trip

# Verify sign and verify
cargo test -p craftec-types sign_and_verify
```

### craftec-crypto

```sh
cargo test -p craftec-crypto
```

Key test areas:
- `KeyStore::new()` generates key on first run, key file exists after creation
- `KeyStore::new()` loads the same `NodeId` across multiple calls
- Sign/verify with the managed keypair
- Wrong message verification fails
- Malformed key file returns `IdentityError`

```sh
# Verify keypair persistence
cargo test -p craftec-crypto loads_existing_keypair

# Verify sign/verify correctness
cargo test -p craftec-crypto sign_and_verify
```

### craftec-obj (CraftOBJ)

```sh
cargo test -p craftec-obj
```

Key test areas:
- `put` stores an object and returns a stable CID
- `get` returns the original bytes for a known CID
- `get` returns `None` for an unknown CID
- BLAKE3 **integrity verification**: if you corrupt a stored file on disk, `get` must return `IntegrityViolation`
- `contains` returns correct results before and after `put`
- `delete` removes an object and subsequent `get` returns `None`
- `list_cids` returns all stored CIDs

**Integrity testing (manual):**

```sh
# After running the test suite, find an object file in target/...
# and flip a byte, then verify that CraftOBJ detects the corruption:
RUST_LOG=craftec_obj=trace cargo test -p craftec-obj integrity -- --nocapture
```

### craftec-rlnc (RLNC / GF(2^8))

```sh
cargo test -p craftec-rlnc
```

Key test areas:
- **GF(2^8) arithmetic** — addition, multiplication, division, identity elements, log/exp table correctness
- **Encoder** — produces exactly `k` coded pieces from `k` source blocks
- **Decoder** — decodes `k` linearly independent coded pieces back to original data
- **Recoder** — produces a new coded piece from a set of existing coded pieces
- **Encode/decode roundtrip** — the most important end-to-end property

**GF(2^8) verification steps:**

```sh
# Run all GF(256) tests
cargo test -p craftec-rlnc gf256 -- --nocapture

# Verify multiplication table
cargo test -p craftec-rlnc mul_table

# Verify distributive law
cargo test -p craftec-rlnc distributive_law

# Run encode/decode roundtrip
cargo test -p craftec-rlnc encode_decode_roundtrip -- --nocapture
```

**RLNC encode/decode roundtrip testing:**

The roundtrip test verifies the core coding property: encode N source blocks into N coded pieces, then decode them back to recover the original bytes exactly.

```sh
cargo test -p craftec-rlnc -- --nocapture 2>&1 | grep -E "encode|decode|roundtrip"
```

Expected output shows:
1. Encoder receives `data` bytes and produces `k` `CodedPiece` values
2. Decoder receives `k` linearly independent pieces and recovers original data
3. `decoded == original_data` assertion passes

### craftec-vfs (CID-VFS)

```sh
cargo test -p craftec-vfs
```

Key test areas:
- `write_page` + `commit` produces a non-zero root CID
- `read_page` after commit returns the written page bytes exactly
- Snapshot isolation: reads through a `Snapshot` see the state at snapshot creation time, not subsequent commits
- `current_root` changes after each `commit`

### craftec-sql (CraftSQL)

```sh
cargo test -p craftec-sql
```

Key test areas:
- `CraftDatabase::open` in-memory mode succeeds
- Schema migration executes without error
- `execute_write` commits a SQL statement and advances the commit hash
- `execute_read` returns rows consistent with previous writes

### craftec-net (P2P Networking)

```sh
cargo test -p craftec-net
```

Key test areas:
- `SwimMembership::new` initialises with empty member list
- `mark_alive` / `mark_suspect` / `mark_dead` transitions are correct
- `alive_members` returns only `Alive` nodes
- `handle_message` on a `WireMessage::Ping` returns a `WireMessage::Pong`
- `DhtProviders` stores and retrieves provider lists by CID

**Network testing with multiple local nodes:**

To test full end-to-end connectivity with three local nodes, use distinct ports:

```sh
# Terminal 1 — bootstrap node (no peers)
mkdir -p /tmp/craftec-node1
RUST_LOG=debug ./target/debug/craftec \
  --data-dir /tmp/craftec-node1 \
  --port 14433

# Terminal 2 — node 2, bootstraps off node 1
mkdir -p /tmp/craftec-node2
RUST_LOG=debug ./target/debug/craftec \
  --data-dir /tmp/craftec-node2 \
  --port 14434 \
  --peers 127.0.0.1:14433

# Terminal 3 — node 3, bootstraps off node 1
mkdir -p /tmp/craftec-node3
RUST_LOG=debug ./target/debug/craftec \
  --data-dir /tmp/craftec-node3 \
  --port 14435 \
  --peers 127.0.0.1:14433
```

After startup you should see `PeerConnected` events in all three log streams and SWIM member counts of 3.

### craftec-health (Health Scanner)

```sh
cargo test -p craftec-health
```

Key test areas:
- `PieceTracker::new()` starts empty
- `record_piece` adds a holder; `available_count` increases
- `remove_node` purges all records for a dead node
- `prune_stale` removes records older than `max_age`
- `HealthScanner::scan_cycle` completes without error on an empty store
- `RepairRequest::Critical` is emitted when `available < k`
- `RepairRequest::Normal` is emitted when `available < target`

**Health scan testing:**

```sh
cargo test -p craftec-health scan_cycle -- --nocapture
```

### craftec-com (CraftCOM / WASM)

```sh
cargo test -p craftec-com
```

Key test areas:
- `ComRuntime::new` initialises Wasmtime engine without error
- `execute_agent` runs a minimal WASM module and returns the correct result
- Fuel exhaustion is detected and reported as `ComError::FuelExhausted`
- `ProgramScheduler::load_program` / `start_program` / `stop_program` lifecycle transitions

---

## Checking Test Coverage

Coverage analysis requires the `llvm-tools` component and `cargo-llvm-cov`:

```sh
# Install cargo-llvm-cov
cargo install cargo-llvm-cov

# Install LLVM tools
rustup component add llvm-tools

# Generate HTML coverage report
cargo llvm-cov --workspace --html --open
```

---

## Linting and Formatting

```sh
# Check formatting (does not modify files)
cargo fmt --all -- --check

# Apply formatting
cargo fmt --all

# Run Clippy lints (treat warnings as errors, as in CI)
cargo clippy --workspace -- -D warnings

# Run Clippy with fix suggestions applied automatically
cargo clippy --workspace --fix
```

---

## CI Test Matrix

The GitHub Actions CI pipeline (`.github/workflows/ci.yml`) runs on every push to `main` and on pull requests:

| Job | Command | Purpose |
|---|---|---|
| `check` | `cargo check --workspace` | Type-check without building artifacts |
| `test` | `cargo test --workspace` | Full test suite |
| `fmt` | `cargo fmt --all -- --check` | Reject unformatted code |
| `clippy` | `cargo clippy --workspace -- -D warnings` | Reject lint warnings |

All jobs use Rust 1.85 (stable). `RUST_LOG=debug` and `RUST_BACKTRACE=1` are set for the test job to maximise diagnostic output on failures.
