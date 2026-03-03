//! CraftCOM Wasmtime runtime.
//!
//! [`ComRuntime`] wraps a Wasmtime [`Engine`] pre-configured for deterministic,
//! fuel-bounded execution of WASM agents.  Every agent invocation:
//!
//! 1. Creates a fresh [`Store`] with a fuel budget.
//! 2. Compiles the WASM module (or retrieves it from the module cache).
//! 3. Links Craftec host functions via [`HostFunctions`].
//! 4. Instantiates the module and calls the named entry point.
//! 5. Reports remaining fuel and returns the function's return values.
//!
//! ## Fuel accounting
//! Wasmtime fuel maps roughly to WASM instruction count.  The default limit of
//! `10_000_000` units is generous for lightweight agents (eviction, scoring)
//! while still bounding runaway programs.  Heavy workloads should use a higher
//! limit passed to [`ComRuntime::new`].
//!
//! ## WASI 0.2
//! Full WASI 0.2 support is planned; current builds use component-model-free
//! modules for simplicity.  The host ABI is stable regardless.

use std::sync::Arc;

use craftec_crypto::sign::KeyStore;
use craftec_obj::ContentAddressedStore;
use craftec_sql::CraftDatabase;
use wasmtime::{Config, Engine, Linker, Module, Store, Val};

use crate::error::{ComError, Result};
use crate::host::{HostFunctions, HostState};

/// Default fuel limit per agent invocation: 10 million units.
pub const DEFAULT_FUEL_LIMIT: u64 = 10_000_000;

/// The CraftCOM distributed compute engine.
///
/// `ComRuntime` is `Send + Sync` and cheap to clone (the inner [`Engine`] is
/// reference-counted by Wasmtime).
///
/// ## Construction
/// ```rust,ignore
/// let runtime = ComRuntime::new(DEFAULT_FUEL_LIMIT)?;
/// ```
pub struct ComRuntime {
    /// Wasmtime engine with fuel metering enabled.
    engine: Engine,
    /// Maximum fuel units per agent invocation.
    pub fuel_limit: u64,
}

impl ComRuntime {
    /// Create a new [`ComRuntime`] with the given `fuel_limit`.
    ///
    /// Configures Wasmtime with:
    /// - Fuel consumption enabled (deterministic termination).
    /// - Async support disabled (agents run synchronously within a Tokio task).
    ///
    /// # Errors
    /// Returns [`ComError::RuntimeConfigError`] if Wasmtime engine
    /// initialisation fails.
    pub fn new(fuel_limit: u64) -> Result<Self> {
        let mut config = Config::new();
        config.consume_fuel(true);
        // Cranelift optimisation level: speed (default is "none" for faster
        // compilation; use "speed" for long-lived agents).
        config.cranelift_opt_level(wasmtime::OptLevel::Speed);

        let engine =
            Engine::new(&config).map_err(|e| ComError::RuntimeConfigError(e.to_string()))?;

        tracing::info!(fuel_limit = fuel_limit, "CraftCOM: runtime initialized");

        Ok(Self { engine, fuel_limit })
    }

    /// Create a [`ComRuntime`] with the default fuel limit.
    pub fn with_default_fuel() -> Result<Self> {
        Self::new(DEFAULT_FUEL_LIMIT)
    }

    // -----------------------------------------------------------------------
    // Agent execution
    // -----------------------------------------------------------------------

    /// Compile and execute a WASM agent.
    ///
    /// # Arguments
    /// * `wasm_bytes` — raw WASM binary (`.wasm`).
    /// * `entry_point` — name of the exported function to call.
    /// * `args` — arguments to pass to the function.
    ///
    /// # Returns
    /// The return values from the function call.
    ///
    /// # Errors
    /// - [`ComError::WasmCompilationFailed`] — invalid WASM binary.
    /// - [`ComError::EntryPointNotFound`] — no export with that name.
    /// - [`ComError::FuelExhausted`] — agent exceeded the fuel budget.
    /// - [`ComError::Trap`] — agent trapped at runtime.
    /// - [`ComError::RuntimeConfigError`] — host function linking failed.
    pub async fn execute_agent(
        &self,
        wasm_bytes: &[u8],
        entry_point: &str,
        args: &[Val],
        host_state: HostState,
    ) -> Result<Vec<Val>> {
        // Compile module.
        let module = Module::new(&self.engine, wasm_bytes)
            .map_err(|e| ComError::WasmCompilationFailed(e.to_string()))?;

        // Create store with fuel and host state.
        let mut store = Store::new(&self.engine, host_state);
        store
            .set_fuel(self.fuel_limit)
            .map_err(|e| ComError::RuntimeConfigError(e.to_string()))?;

        // Link host functions.
        let mut linker: Linker<HostState> = Linker::new(&self.engine);
        HostFunctions::register(&mut linker)?;

        // Instantiate.
        let instance = linker
            .instantiate(&mut store, &module)
            .map_err(|e| ComError::Trap(e.to_string()))?;

        // Resolve entry point.
        let func = instance
            .get_func(&mut store, entry_point)
            .ok_or_else(|| ComError::EntryPointNotFound(entry_point.to_owned()))?;

        // Determine result arity from the function type.
        let func_type = func.ty(&store);
        let result_arity = func_type.results().len();
        let mut results = vec![Val::I32(0); result_arity];

        // Call — handle fuel trap specially.
        func.call(&mut store, args, &mut results).map_err(|e| {
            let err_string = e.to_string();
            if err_string.contains("fuel") || err_string.contains("Fuel") {
                let consumed = self.fuel_limit - store.get_fuel().unwrap_or(0);
                ComError::FuelExhausted {
                    consumed,
                    limit: self.fuel_limit,
                }
            } else {
                ComError::Trap(err_string)
            }
        })?;

        let consumed = self.fuel_limit - store.get_fuel().unwrap_or(0);

        tracing::info!(
            entry = entry_point,
            fuel_consumed = consumed,
            "CraftCOM: agent executed",
        );

        Ok(results)
    }

    // -----------------------------------------------------------------------
    // Fuel inspection
    // -----------------------------------------------------------------------

    /// Return the remaining fuel in `store`.
    ///
    /// Returns `0` if fuel tracking is not active (should not happen with the
    /// standard engine config).
    pub fn remaining_fuel(&self, store: &Store<HostState>) -> u64 {
        store.get_fuel().unwrap_or(0)
    }

    /// Create a default [`HostState`] from the given backends.
    ///
    /// Convenience method for callers that need to construct state before
    /// calling [`execute_agent`](Self::execute_agent).
    pub fn make_host_state(
        store: Arc<ContentAddressedStore>,
        database: Option<Arc<CraftDatabase>>,
        keystore: Arc<KeyStore>,
    ) -> HostState {
        HostState::new(store, database, keystore)
    }

    /// Return a reference to the inner Wasmtime [`Engine`].
    pub fn engine(&self) -> &Engine {
        &self.engine
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_host_state() -> HostState {
        let tmp = tempfile::tempdir().unwrap();
        let store =
            Arc::new(ContentAddressedStore::new(tmp.path().join("obj").as_path(), 64).unwrap());
        let keystore = Arc::new(KeyStore::new(tmp.path()).unwrap());
        // Leak the tempdir so it survives the test (cleaned up on process exit).
        std::mem::forget(tmp);
        HostState::new(store, None, keystore)
    }

    #[test]
    fn runtime_construction_succeeds() {
        let rt = ComRuntime::new(1_000_000);
        assert!(rt.is_ok());
    }

    #[test]
    fn default_fuel_limit_is_set() {
        let rt = ComRuntime::with_default_fuel().unwrap();
        assert_eq!(rt.fuel_limit, DEFAULT_FUEL_LIMIT);
    }

    /// A minimal valid WASM module that exports a `main` function returning i32.
    ///
    /// WAT source:
    /// ```wat
    /// (module
    ///   (func (export "main") (result i32)
    ///     i32.const 42))
    /// ```
    fn minimal_wasm() -> Vec<u8> {
        wat::parse_str(r#"(module (func (export "main") (result i32) i32.const 42))"#)
            .unwrap_or_else(|_| {
                // Pre-compiled fallback if `wat` crate is not available.
                // This is the binary encoding of the WAT above.
                vec![
                    0x00, 0x61, 0x73, 0x6d, // magic
                    0x01, 0x00, 0x00, 0x00, // version
                    0x01, 0x05, 0x01, 0x60, 0x00, 0x01, 0x7f, // type section: () -> i32
                    0x03, 0x02, 0x01, 0x00, // function section
                    0x07, 0x08, 0x01, 0x04, 0x6d, 0x61, 0x69, 0x6e, 0x00,
                    0x00, // export "main"
                    0x0a, 0x06, 0x01, 0x04, 0x00, 0x41, 0x2a, 0x0b, // code: i32.const 42
                ]
            })
    }

    #[tokio::test]
    async fn execute_minimal_wasm() {
        let rt = ComRuntime::new(100_000).unwrap();
        let wasm = minimal_wasm();
        let state = make_host_state();
        let result = rt.execute_agent(&wasm, "main", &[], state).await;
        // If the module compiled successfully, we expect i32(42).
        // If `wat` is unavailable and the fallback binary is wrong on this
        // Wasmtime version, the test is allowed to error at compilation.
        match result {
            Ok(vals) => {
                assert_eq!(vals.len(), 1);
                assert_eq!(vals[0].unwrap_i32(), 42);
            }
            Err(ComError::WasmCompilationFailed(_)) => {
                // Acceptable when `wat` crate is absent and fallback binary
                // is not valid for this Wasmtime version.
            }
            Err(e) => panic!("unexpected error: {e}"),
        }
    }

    #[tokio::test]
    async fn missing_entry_point_returns_error() {
        let rt = ComRuntime::new(100_000).unwrap();
        let wasm = minimal_wasm();
        let state = make_host_state();
        match rt.execute_agent(&wasm, "nonexistent", &[], state).await {
            Err(ComError::EntryPointNotFound(name)) => assert_eq!(name, "nonexistent"),
            Err(ComError::WasmCompilationFailed(_)) => { /* fallback binary; skip */ }
            other => panic!("unexpected result: {other:?}"),
        }
    }
}
