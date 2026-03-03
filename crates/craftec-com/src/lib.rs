//! `craftec-com` — CraftCOM: distributed compute engine for Craftec.
//!
//! CraftCOM enables network-owned programs (*agents*) to run as sandboxed
//! WASM processes on Craftec nodes.  Agents interact with the network through
//! a well-defined host ABI rather than arbitrary syscalls, ensuring security
//! and determinism.
//!
//! ## Architecture
//!
//! ```text
//!  ┌─────────────────────────────────────────────────────────┐
//!  │                      Node binary                         │
//!  │  ┌──────────────────┐   ┌──────────────────────────────┐│
//!  │  │ ProgramScheduler │──►│         ComRuntime            ││
//!  │  │  (kernel-level)  │   │  ┌────────────────────────┐  ││
//!  │  └──────────────────┘   │  │   Wasmtime Engine      │  ││
//!  │                         │  │   (fuel-bounded)       │  ││
//!  │                         │  └──────────┬─────────────┘  ││
//!  │                         │             │ host ABI        ││
//!  │                         │  ┌──────────▼─────────────┐  ││
//!  │                         │  │   HostFunctions         │  ││
//!  │                         │  │  craft_store_get/put    │  ││
//!  │                         │  │  craft_sql_query        │  ││
//!  │                         │  │  craft_sign / craft_log │  ││
//!  │                         │  └────────────────────────┘  ││
//!  │                         └──────────────────────────────┘│
//!  └─────────────────────────────────────────────────────────┘
//! ```
//!
//! ## Key properties
//! - **Sandboxed** — agents cannot escape the Wasmtime sandbox.
//! - **Fuel-bounded** — every agent invocation is capped at a configurable
//!   instruction budget, preventing runaway loops.
//! - **Network-owned** — WASM binaries are stored in CraftOBJ and identified
//!   by CID, not by local filesystem paths.
//! - **WASI 0.2** — agents use WASI component model interfaces (roadmap).
//!
//! ## Crate layout
//! | Module | Responsibility |
//! |---|---|
//! | [`runtime`] | [`ComRuntime`] — Wasmtime engine wrapper |
//! | [`host`] | [`HostFunctions`] — host ABI exposed to agents |
//! | [`scheduler`] | [`ProgramScheduler`] — kernel-level lifecycle manager |
//! | [`agent`] | [`Agent`] + [`AgentKind`] — agent descriptors |
//! | [`error`] | [`ComError`] enum and [`Result`] alias |

pub mod agent;
pub mod error;
pub mod host;
pub mod runtime;
pub mod scheduler;

// Convenience re-exports.
pub use agent::{Agent, AgentKind};
pub use error::{ComError, Result};
pub use host::{HostFunctions, HostState};
pub use runtime::{ComRuntime, DEFAULT_FUEL_LIMIT};
pub use scheduler::{ProgramScheduler, ProgramState};
