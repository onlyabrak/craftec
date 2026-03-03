# Development Guide

This document describes how to set up the development environment, build the project, follow code conventions, add new crates, and navigate the CI pipeline.

---

## Setting Up the Development Environment

### 1. Install Rust

```sh
# Install rustup (if not already installed)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Install Rust 1.85 stable
rustup install 1.85
rustup default stable

# Verify
rustc --version   # rustc 1.85.x
cargo --version   # cargo 1.85.x
```

### 2. Install Optional Tooling

```sh
# rustfmt and clippy (usually bundled with stable; add if missing)
rustup component add rustfmt clippy

# cargo-llvm-cov for test coverage
cargo install cargo-llvm-cov

# cargo-watch for auto-rebuild on file changes
cargo install cargo-watch

# cargo-expand to inspect macro expansions
cargo install cargo-expand
```

### 3. Clone and Build

```sh
git clone https://github.com/onlyabrak/craftec
cd craftec
cargo build
```

The first build downloads all dependencies (~150 crates) and may take 3–5 minutes.
Subsequent incremental builds take seconds.

---

## Workspace Structure

Craftec uses a Cargo workspace with a single root `Cargo.toml` listing all member crates.
All crates inherit `version`, `edition`, `license`, and common dependencies from the workspace manifest.

```
craftec/
├── Cargo.toml          ← Workspace manifest: members, shared deps, profiles
├── Cargo.lock          ← Locked versions (committed — this workspace contains a binary)
├── README.md
├── docs/
│   ├── TESTING.md
│   └── DEVELOPMENT.md  ← This file
└── crates/
    ├── craftec-types/  ← Foundation: shared types, no internal deps
    ├── craftec-crypto/ ← Depends on: craftec-types
    ├── craftec-obj/    ← Depends on: craftec-types
    ├── craftec-rlnc/   ← Depends on: craftec-types
    ├── craftec-vfs/    ← Depends on: craftec-types, craftec-obj
    ├── craftec-sql/    ← Depends on: craftec-types, craftec-vfs
    ├── craftec-net/    ← Depends on: craftec-types, craftec-crypto
    ├── craftec-health/ ← Depends on: craftec-types, craftec-obj
    ├── craftec-com/    ← Depends on: craftec-types
    └── craftec-node/   ← Binary: depends on all crates above
```

**Dependency rule:** crates at lower layers must never depend on crates at higher layers. `craftec-types` has no internal dependencies. `craftec-node` depends on everything.

---

## Build Commands

```sh
# Debug build (fast compile, slow binary — use for development)
cargo build

# Release build (slow compile, fast binary — use for production)
cargo build --release

# Build a single crate
cargo build -p craftec-rlnc

# Build only the binary
cargo build -p craftec-node

# Check types without producing artifacts (very fast)
cargo check --workspace

# Watch for changes and rebuild automatically
cargo watch -x "build"
cargo watch -x "test"
```

---

## Running the Node

```sh
# Debug binary
./target/debug/craftec

# Release binary
./target/release/craftec

# With trace logging
RUST_LOG=trace ./target/debug/craftec

# With per-crate log levels
RUST_LOG=craftec_net=debug,craftec_health=info,warn ./target/debug/craftec

# With full panic backtrace
RUST_BACKTRACE=full ./target/debug/craftec
```

---

## Code Conventions

### Tracing Patterns

All logging uses `tracing` macros — never `println!` or `eprintln!` in library code.

```rust
// Startup / shutdown events: info!
tracing::info!(port = config.listen_port, "Starting listener");

// Internal state transitions: debug!
tracing::debug!(cid = %cid, "Object stored to disk");

// High-frequency per-packet / per-piece events: trace!
tracing::trace!(bytes = data.len(), peer = %peer_id, "Sending coded piece");

// Recoverable errors or unexpected but non-fatal conditions: warn!
tracing::warn!(error = %e, path = %path.display(), "Failed to read node.lock");

// Fatal errors (always followed by ? or .unwrap() in tests): error!
tracing::error!(error = %e, "Critical failure — cannot continue");
```

Structured fields (key = value) are preferred over format strings. Use `%` for `Display` and `?` for `Debug`:

```rust
// Good
tracing::info!(node_id = %keystore.node_id(), bytes = data.len(), "Stored object");

// Avoid
tracing::info!("Stored object: node={} bytes={}", keystore.node_id(), data.len());
```

### Error Handling

Library crates define a crate-specific `Error` enum using `thiserror` and a `Result<T>` alias:

```rust
// In craftec-obj/src/error.rs
#[derive(Debug, thiserror::Error)]
pub enum ObjError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("integrity violation for CID {cid}: expected {expected}, got {actual}")]
    IntegrityViolation { cid: Cid, expected: String, actual: String },
}

pub type Result<T> = std::result::Result<T, ObjError>;
```

Binary crate (`craftec-node`) uses `anyhow::Result` for ergonomic error propagation:

```rust
use anyhow::{Context, Result};

fn load_config() -> Result<NodeConfig> {
    NodeConfig::load(&path).context("failed to load node config")
}
```

Use `?` for propagation everywhere. Avoid `.unwrap()` except in test code and infallible invariants.

### Naming Conventions

| Item | Convention | Example |
|---|---|---|
| Types | `PascalCase` | `ContentAddressedStore`, `NodeKeypair` |
| Functions / methods | `snake_case` | `encode_pieces`, `run_swim_loop` |
| Constants | `SCREAMING_SNAKE_CASE` | `DEFAULT_FUEL_LIMIT`, `ALPN_CRAFTEC` |
| Modules | `snake_case` | `event_bus`, `piece_tracker` |
| Crates | `kebab-case` | `craftec-rlnc`, `craftec-net` |
| Test functions | Descriptive `snake_case` | `encode_decode_roundtrip`, `bad_key_file_returns_error` |

### Module Organisation

Each crate follows this layout:

```
crates/craftec-<name>/
├── Cargo.toml
└── src/
    ├── lib.rs          ← Public re-exports + crate-level doc comment
    ├── error.rs        ← Error enum + Result alias
    ├── <primary>.rs    ← Main public type
    └── <helper>.rs     ← Supporting types
```

`lib.rs` re-exports the primary public surface:

```rust
pub use error::{MyError, Result};
pub use primary::MyPrimaryType;
```

---

## Adding a New Crate to the Workspace

1. **Create the crate directory:**
   ```sh
   cargo new --lib crates/craftec-mynewcrate
   ```

2. **Set up `Cargo.toml`** to inherit workspace settings:
   ```toml
   [package]
   name = "craftec-mynewcrate"
   version.workspace = true
   edition.workspace = true
   license.workspace = true

   [dependencies]
   craftec-types = { workspace = true }
   thiserror = { workspace = true }
   tracing = { workspace = true }
   ```

3. **Register in the workspace `Cargo.toml`:**
   ```toml
   [workspace]
   members = [
       # ... existing members ...
       "crates/craftec-mynewcrate",
   ]

   [workspace.dependencies]
   craftec-mynewcrate = { path = "crates/craftec-mynewcrate" }
   ```

4. **Add to `craftec-node/Cargo.toml`** if it should be composed into the binary:
   ```toml
   [dependencies]
   craftec-mynewcrate = { workspace = true }
   ```

5. **Write `src/lib.rs`** with a crate-level doc comment and public re-exports.

6. **Write `src/error.rs`** with a `thiserror`-derived error enum.

7. **Add tests** in `#[cfg(test)]` modules covering the primary public API.

---

## Debugging

### Log Levels

```sh
# Trace everything
RUST_LOG=trace cargo run

# Trace only specific modules
RUST_LOG=craftec_net::swim=trace,craftec_health=debug,info cargo run

# Debug only the node orchestration
RUST_LOG=craftec_node=debug,info cargo run
```

### Panic Backtraces

```sh
# Short backtrace
RUST_BACKTRACE=1 cargo run

# Full backtrace (includes standard library frames)
RUST_BACKTRACE=full cargo run
```

### Expanding Macros

```sh
# Expand a specific module's macros (requires cargo-expand)
cargo expand -p craftec-types config
```

### Inspecting the Binary

```sh
# List exported symbols
nm -D target/release/craftec | grep craftec

# Check binary size
ls -lh target/release/craftec
```

---

## CI Pipeline

The GitHub Actions pipeline (`.github/workflows/ci.yml`) runs four parallel jobs on every `push` to `main` and every pull request:

| Job | Command | Fails on |
|---|---|---|
| **check** | `cargo check --workspace` | Type errors, unresolved imports |
| **test** | `cargo test --workspace` | Test assertion failures, panics |
| **fmt** | `cargo fmt --all -- --check` | Unformatted code |
| **clippy** | `cargo clippy --workspace -- -D warnings` | Lint warnings |

All jobs run on `ubuntu-latest` with Rust 1.85. The test job sets `RUST_LOG=debug` and `RUST_BACKTRACE=1` for maximum diagnostic output.

**To reproduce CI locally:**

```sh
cargo check --workspace
RUST_LOG=debug RUST_BACKTRACE=1 cargo test --workspace
cargo fmt --all -- --check
cargo clippy --workspace -- -D warnings
```

---

## Git Workflow

### Branch Naming

| Type | Pattern | Example |
|---|---|---|
| Feature | `feat/<short-description>` | `feat/swim-suspicion` |
| Bug fix | `fix/<short-description>` | `fix/bloom-filter-false-positive` |
| Refactor | `refactor/<short-description>` | `refactor/event-bus-capacity` |
| Documentation | `docs/<short-description>` | `docs/testing-guide` |
| Release | `release/<version>` | `release/0.2.0` |

### Commit Messages

Follow conventional commits format:

```
<type>(<scope>): <short description>

[optional body]

[optional footer]
```

Examples:
```
feat(craftec-rlnc): add recoder for in-network re-encoding
fix(craftec-obj): detect BLAKE3 integrity violations on cached reads
docs(craftec-net): document SWIM failure detection thresholds
refactor(craftec-node): extract event dispatch into dedicated module
```

### Pull Request Process

1. Branch off `main` using the naming convention above.
2. Write code, tests, and documentation.
3. Run `cargo fmt --all` and `cargo clippy --workspace -- -D warnings` locally.
4. Ensure `cargo test --workspace` passes locally.
5. Push the branch and open a PR against `main`.
6. All four CI jobs must pass before merging.
7. Squash-merge PRs to keep `main` history linear.

---

## Release Process

1. Update version in workspace `Cargo.toml` (`[workspace.package] version = "x.y.z"`).
2. All crates inherit the version automatically.
3. Tag the commit: `git tag v0.x.y && git push origin v0.x.y`.
4. Build the release binary: `cargo build --release`.
5. The release binary at `target/release/craftec` is ready for distribution.
