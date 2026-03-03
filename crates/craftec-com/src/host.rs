//! Host functions exposed to WASM agents via the Craftec ABI.
//!
//! These functions form the Craftec host ABI: the set of capabilities that
//! network-owned WASM programs can invoke from their sandboxed environment.
//! Every function is declared with a canonical `i32`/`i64` interface so that
//! WASM modules compiled from any language toolchain can call them.
//!
//! ## Memory convention
//! All pointer/length pairs follow the standard WASM linear-memory pattern:
//! - `*_ptr: i32` — byte offset in the WASM module's linear memory.
//! - `*_len: i32` — byte length of the data at that offset.
//! - Return `i64` — the high 32 bits carry the byte offset of the result; the
//!   low 32 bits carry the result length.  A negative value indicates an error
//!   (error code in the high 32 bits).
//!
//! ## ABI stability
//! The function names, signatures, and return conventions documented here are
//! stable once stabilised.  Breaking changes require a new ABI version prefix.
//!
//! ## Current functions
//! | Name | Description |
//! |---|---|
//! | `craft_store_get` | Read an object from CraftOBJ by CID |
//! | `craft_store_put` | Write an object to CraftOBJ, return its CID |
//! | `craft_sql_query` | Execute a read-only CraftSQL query |
//! | `craft_sign` | Sign a message with the node's Ed25519 key |
//! | `craft_log` | Emit a tracing log line from inside the sandbox |

use wasmtime::{Caller, Linker};

use crate::error::ComError;

/// Namespace used when registering host functions with the Wasmtime linker.
pub const HOST_MODULE: &str = "craftec";

/// Marker struct that groups all host function implementations.
///
/// The actual closures are registered with the Wasmtime [`Linker`] via
/// [`HostFunctions::register`].
pub struct HostFunctions;

impl HostFunctions {
    /// Register all Craftec host functions into the given Wasmtime [`Linker`].
    ///
    /// This must be called before instantiating any WASM module that imports
    /// Craftec host functions.
    ///
    /// # Errors
    /// Returns [`ComError::RuntimeConfigError`] if any function fails to
    /// register (e.g., a duplicate name).
    pub fn register(linker: &mut Linker<()>) -> Result<(), ComError> {
        Self::register_store_get(linker)?;
        Self::register_store_put(linker)?;
        Self::register_sql_query(linker)?;
        Self::register_sign(linker)?;
        Self::register_log(linker)?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // craft_store_get
    // -----------------------------------------------------------------------

    /// `craft_store_get(cid_ptr: i32, cid_len: i32) -> i64`
    ///
    /// Read the object identified by the 32-byte CID at `[cid_ptr, cid_ptr+cid_len)`
    /// from CraftOBJ.
    ///
    /// ## Return value
    /// `(result_ptr << 32) | result_len` on success, or a negative error code.
    ///
    /// ## Notes
    /// In the full implementation the returned bytes are written into the
    /// module's scratch buffer managed by the host.  The stub below logs the
    /// call and returns 0 (success, empty payload).
    fn register_store_get(linker: &mut Linker<()>) -> Result<(), ComError> {
        linker
            .func_wrap(HOST_MODULE, "craft_store_get", |mut _caller: Caller<'_, ()>, cid_ptr: i32, cid_len: i32| -> i64 {
                tracing::debug!(
                    cid_ptr = cid_ptr,
                    cid_len = cid_len,
                    "craft_store_get: called",
                );
                // Stub: returns 0 (empty result, no error).
                0i64
            })
            .map_err(|e| ComError::RuntimeConfigError(e.to_string()))?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // craft_store_put
    // -----------------------------------------------------------------------

    /// `craft_store_put(data_ptr: i32, data_len: i32) -> i64`
    ///
    /// Write `data_len` bytes starting at `data_ptr` in the module's linear
    /// memory to CraftOBJ.
    ///
    /// ## Return value
    /// `(cid_ptr << 32) | 32` where `cid_ptr` is the offset of the 32-byte
    /// CID written into the host scratch buffer, or a negative error code.
    fn register_store_put(linker: &mut Linker<()>) -> Result<(), ComError> {
        linker
            .func_wrap(HOST_MODULE, "craft_store_put", |mut _caller: Caller<'_, ()>, data_ptr: i32, data_len: i32| -> i64 {
                tracing::debug!(
                    data_ptr = data_ptr,
                    data_len = data_len,
                    "craft_store_put: called",
                );
                // Stub: returns 0.
                0i64
            })
            .map_err(|e| ComError::RuntimeConfigError(e.to_string()))?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // craft_sql_query
    // -----------------------------------------------------------------------

    /// `craft_sql_query(sql_ptr: i32, sql_len: i32) -> i64`
    ///
    /// Execute a read-only CraftSQL query.  The SQL string lives at
    /// `[sql_ptr, sql_ptr+sql_len)` in the module's linear memory.
    ///
    /// ## Return value
    /// `(rows_ptr << 32) | rows_len` pointing to a postcard-encoded
    /// `Vec<Vec<ColumnValue>>` in the host scratch buffer, or a negative error.
    fn register_sql_query(linker: &mut Linker<()>) -> Result<(), ComError> {
        linker
            .func_wrap(HOST_MODULE, "craft_sql_query", |mut _caller: Caller<'_, ()>, sql_ptr: i32, sql_len: i32| -> i64 {
                tracing::debug!(
                    sql_ptr = sql_ptr,
                    sql_len = sql_len,
                    "craft_sql_query: called",
                );
                // Stub: returns 0 (empty result set).
                0i64
            })
            .map_err(|e| ComError::RuntimeConfigError(e.to_string()))?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // craft_sign
    // -----------------------------------------------------------------------

    /// `craft_sign(msg_ptr: i32, msg_len: i32) -> i64`
    ///
    /// Sign the `msg_len` bytes at `msg_ptr` with the node's Ed25519 signing
    /// key.  The 64-byte signature is written to the host scratch buffer.
    ///
    /// ## Return value
    /// `(sig_ptr << 32) | 64` on success, or a negative error code.
    ///
    /// ## Security note
    /// Agents may only sign messages whose format matches the Craftec signing
    /// policy; the host validates the message domain before signing.
    fn register_sign(linker: &mut Linker<()>) -> Result<(), ComError> {
        linker
            .func_wrap(HOST_MODULE, "craft_sign", |mut _caller: Caller<'_, ()>, msg_ptr: i32, msg_len: i32| -> i64 {
                tracing::debug!(
                    msg_ptr = msg_ptr,
                    msg_len = msg_len,
                    "craft_sign: called",
                );
                // Stub: returns 0.
                0i64
            })
            .map_err(|e| ComError::RuntimeConfigError(e.to_string()))?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // craft_log
    // -----------------------------------------------------------------------

    /// `craft_log(level: i32, msg_ptr: i32, msg_len: i32)`
    ///
    /// Emit a structured tracing log line from inside the WASM sandbox.
    ///
    /// ## Parameters
    /// * `level` — log level: `0` = error, `1` = warn, `2` = info,
    ///   `3` = debug, `4` = trace.
    /// * `msg_ptr`, `msg_len` — UTF-8 message bytes in linear memory.
    fn register_log(linker: &mut Linker<()>) -> Result<(), ComError> {
        linker
            .func_wrap(HOST_MODULE, "craft_log", |mut caller: Caller<'_, ()>, level: i32, msg_ptr: i32, msg_len: i32| {
                // Read message bytes from WASM linear memory.
                let msg = if let Some(mem) = caller.get_export("memory")
                    .and_then(|e| e.into_memory())
                {
                    let data = mem.data(&caller);
                    let start = msg_ptr as usize;
                    let end = start.saturating_add(msg_len as usize);
                    if end <= data.len() {
                        String::from_utf8_lossy(&data[start..end]).into_owned()
                    } else {
                        "<out-of-bounds>".to_owned()
                    }
                } else {
                    "<no-memory>".to_owned()
                };

                match level {
                    0 => tracing::error!(target: "wasm_agent", "{}", msg),
                    1 => tracing::warn! (target: "wasm_agent", "{}", msg),
                    2 => tracing::info! (target: "wasm_agent", "{}", msg),
                    3 => tracing::debug!(target: "wasm_agent", "{}", msg),
                    _ => tracing::trace!(target: "wasm_agent", "{}", msg),
                }
            })
            .map_err(|e| ComError::RuntimeConfigError(e.to_string()))?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use wasmtime::{Engine, Linker};

    fn make_linker() -> Linker<()> {
        let engine = Engine::default();
        Linker::new(&engine)
    }

    #[test]
    fn all_host_functions_register_without_error() {
        let mut linker = make_linker();
        HostFunctions::register(&mut linker)
            .expect("host function registration should succeed");
    }

    #[test]
    fn duplicate_registration_returns_error() {
        let mut linker = make_linker();
        HostFunctions::register(&mut linker).unwrap();
        // Second registration of the same names should fail (duplicate).
        let result = HostFunctions::register(&mut linker);
        assert!(result.is_err(), "duplicate registration must be detected");
    }
}
