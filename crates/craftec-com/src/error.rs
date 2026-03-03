//! Error types for CraftCOM.
//!
//! All fallible operations in `craftec-com` return [`ComError`] or the
//! crate-local [`Result`] alias.

use craftec_types::Cid;
use thiserror::Error;

/// Errors that can arise within the CraftCOM compute engine.
#[derive(Debug, Error)]
pub enum ComError {
    /// WASM module compilation failed (syntax or validation error in the binary).
    #[error("WASM compilation failed: {0}")]
    WasmCompilationFailed(String),

    /// The WASM agent consumed all available fuel before completing.
    ///
    /// The fuel limit is configured on the [`ComRuntime`](crate::runtime::ComRuntime)
    /// and can be tuned per workload.
    #[error("fuel exhausted after consuming {consumed} units (limit: {limit})")]
    FuelExhausted { consumed: u64, limit: u64 },

    /// A host function called from within a WASM agent returned an error.
    #[error("host function '{function}' failed: {reason}")]
    HostFunctionError { function: String, reason: String },

    /// The requested program CID is not tracked by the scheduler.
    #[error("program not found: {0}")]
    ProgramNotFound(Cid),

    /// A scheduler operation failed (load, start, stop).
    #[error("scheduler error: {0}")]
    SchedulerError(String),

    /// Wasmtime engine configuration error.
    #[error("runtime configuration error: {0}")]
    RuntimeConfigError(String),

    /// The WASM entry point function was not found in the module.
    #[error("entry point '{0}' not found in WASM module")]
    EntryPointNotFound(String),

    /// The WASM instance trapped during execution.
    #[error("WASM trap: {0}")]
    Trap(String),

    /// An error propagated from the CraftSQL layer.
    #[error("CraftSQL error: {0}")]
    SqlError(#[from] craftec_sql::SqlError),
}

/// Crate-local result alias.
pub type Result<T, E = ComError> = std::result::Result<T, E>;
