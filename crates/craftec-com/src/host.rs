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
//! - Return `i64` — non-negative value is the byte length of the result
//!   stored in the host scratch buffer; negative value indicates an error code.
//!
//! Results are stored in `HostState::scratch`.  The WASM module copies them
//! into its own linear memory via `craft_read_result(dst_ptr, offset, len)`.
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
//! | `craft_read_result` | Copy result bytes from host scratch to WASM memory |
//! | `craft_log` | Emit a tracing log line from inside the sandbox |

use std::sync::Arc;

use craftec_crypto::sign::KeyStore;
use craftec_obj::ContentAddressedStore;
use craftec_sql::CraftDatabase;
use craftec_types::Cid;
use wasmtime::{Caller, Linker};

use crate::error::ComError;

/// Namespace used when registering host functions with the Wasmtime linker.
pub const HOST_MODULE: &str = "craftec";

/// Maximum `craft_sign` calls per agent invocation (rate limit per spec §40).
const SIGN_RATE_LIMIT: u32 = 10;

// ---------------------------------------------------------------------------
// HostState
// ---------------------------------------------------------------------------

/// Shared state passed into every WASM agent invocation via Wasmtime's `Store`.
///
/// Each `execute_agent` call creates a fresh `HostState` so that agents
/// cannot leak state between invocations.
pub struct HostState {
    /// Content-addressed object store (CraftOBJ).
    pub store: Arc<ContentAddressedStore>,
    /// SQL database (optional — not all agents need SQL).
    pub database: Option<Arc<CraftDatabase>>,
    /// Ed25519 signing key store.
    pub keystore: Arc<KeyStore>,
    /// Host-side scratch buffer for returning results to the WASM module.
    pub scratch: Vec<u8>,
    /// Number of `craft_sign` calls in this invocation (rate-limited).
    pub sign_count: u32,
}

impl HostState {
    /// Create a new `HostState` with the given backends.
    pub fn new(
        store: Arc<ContentAddressedStore>,
        database: Option<Arc<CraftDatabase>>,
        keystore: Arc<KeyStore>,
    ) -> Self {
        Self {
            store,
            database,
            keystore,
            scratch: Vec::new(),
            sign_count: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// HostFunctions
// ---------------------------------------------------------------------------

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
    pub fn register(linker: &mut Linker<HostState>) -> Result<(), ComError> {
        Self::register_store_get(linker)?;
        Self::register_store_put(linker)?;
        Self::register_sql_query(linker)?;
        Self::register_sign(linker)?;
        Self::register_read_result(linker)?;
        Self::register_log(linker)?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // craft_store_get
    // -----------------------------------------------------------------------

    /// `craft_store_get(cid_ptr: i32, cid_len: i32) -> i64`
    ///
    /// Read the object identified by the 32-byte CID at `[cid_ptr, cid_ptr+cid_len)`
    /// from CraftOBJ.  The result bytes are stored in the host scratch buffer.
    ///
    /// ## Return value
    /// Non-negative: byte length of the result in the scratch buffer.
    /// Negative: error code (-1 = not found, -2 = bad CID, -3 = I/O error).
    fn register_store_get(linker: &mut Linker<HostState>) -> Result<(), ComError> {
        linker
            .func_wrap(
                HOST_MODULE,
                "craft_store_get",
                |mut caller: Caller<'_, HostState>, cid_ptr: i32, cid_len: i32| -> i64 {
                    // Read CID bytes from WASM memory.
                    let mem = match caller.get_export("memory").and_then(|e| e.into_memory()) {
                        Some(m) => m,
                        None => return -3,
                    };
                    let data = mem.data(&caller);
                    let start = cid_ptr as usize;
                    let end = start.saturating_add(cid_len as usize);
                    if end > data.len() || cid_len != 32 {
                        return -2;
                    }
                    let mut cid_bytes = [0u8; 32];
                    cid_bytes.copy_from_slice(&data[start..end]);
                    let cid = Cid::from_bytes(cid_bytes);

                    let store = caller.data().store.clone();

                    // Perform the async get on the current tokio runtime.
                    let result = tokio::task::block_in_place(|| {
                        tokio::runtime::Handle::current().block_on(store.get(&cid))
                    });

                    match result {
                        Ok(Some(bytes)) => {
                            let len = bytes.len();
                            caller.data_mut().scratch = bytes.to_vec();
                            tracing::debug!(cid = %cid, len, "craft_store_get: success");
                            len as i64
                        }
                        Ok(None) => {
                            tracing::debug!(cid = %cid, "craft_store_get: not found");
                            -1
                        }
                        Err(e) => {
                            tracing::warn!(cid = %cid, error = %e, "craft_store_get: error");
                            -3
                        }
                    }
                },
            )
            .map_err(|e| ComError::RuntimeConfigError(e.to_string()))?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // craft_store_put
    // -----------------------------------------------------------------------

    /// `craft_store_put(data_ptr: i32, data_len: i32) -> i64`
    ///
    /// Write `data_len` bytes starting at `data_ptr` in the module's linear
    /// memory to CraftOBJ.  The resulting 32-byte CID is stored in the
    /// host scratch buffer.
    ///
    /// ## Return value
    /// `32` on success (length of the CID in scratch), or negative error code.
    fn register_store_put(linker: &mut Linker<HostState>) -> Result<(), ComError> {
        linker
            .func_wrap(
                HOST_MODULE,
                "craft_store_put",
                |mut caller: Caller<'_, HostState>, data_ptr: i32, data_len: i32| -> i64 {
                    let mem = match caller.get_export("memory").and_then(|e| e.into_memory()) {
                        Some(m) => m,
                        None => return -3,
                    };
                    let data = mem.data(&caller);
                    let start = data_ptr as usize;
                    let end = start.saturating_add(data_len as usize);
                    if end > data.len() {
                        return -2;
                    }
                    let payload = data[start..end].to_vec();

                    let store = caller.data().store.clone();

                    let result = tokio::task::block_in_place(|| {
                        tokio::runtime::Handle::current().block_on(store.put(&payload))
                    });

                    match result {
                        Ok(cid) => {
                            caller.data_mut().scratch = cid.as_bytes().to_vec();
                            tracing::debug!(cid = %cid, "craft_store_put: success");
                            32
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "craft_store_put: error");
                            -3
                        }
                    }
                },
            )
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
    /// Non-negative: byte length of the postcard-encoded result in scratch.
    /// Negative: error code (-1 = no database, -2 = bad SQL, -3 = I/O error).
    fn register_sql_query(linker: &mut Linker<HostState>) -> Result<(), ComError> {
        linker
            .func_wrap(
                HOST_MODULE,
                "craft_sql_query",
                |mut caller: Caller<'_, HostState>, sql_ptr: i32, sql_len: i32| -> i64 {
                    let mem = match caller.get_export("memory").and_then(|e| e.into_memory()) {
                        Some(m) => m,
                        None => return -3,
                    };
                    let data = mem.data(&caller);
                    let start = sql_ptr as usize;
                    let end = start.saturating_add(sql_len as usize);
                    if end > data.len() {
                        return -2;
                    }
                    let sql = match std::str::from_utf8(&data[start..end]) {
                        Ok(s) => s.to_owned(),
                        Err(_) => return -2,
                    };

                    let db = match &caller.data().database {
                        Some(db) => db.clone(),
                        None => return -1,
                    };

                    let result = tokio::task::block_in_place(|| {
                        tokio::runtime::Handle::current().block_on(db.query(&sql))
                    });

                    match result {
                        Ok(rows) => match postcard::to_allocvec(&rows) {
                            Ok(encoded) => {
                                let len = encoded.len();
                                caller.data_mut().scratch = encoded;
                                tracing::debug!(
                                    sql = sql,
                                    rows = rows.len(),
                                    "craft_sql_query: success"
                                );
                                len as i64
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, "craft_sql_query: serialization error");
                                -3
                            }
                        },
                        Err(e) => {
                            tracing::warn!(sql = sql, error = %e, "craft_sql_query: error");
                            -2
                        }
                    }
                },
            )
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
    /// Rate-limited to [`SIGN_RATE_LIMIT`] calls per invocation (spec §40).
    ///
    /// ## Return value
    /// `64` on success (length of the Ed25519 signature in scratch).
    /// Negative: error code (-1 = rate limit, -2 = bad input, -3 = I/O error).
    fn register_sign(linker: &mut Linker<HostState>) -> Result<(), ComError> {
        linker
            .func_wrap(
                HOST_MODULE,
                "craft_sign",
                |mut caller: Caller<'_, HostState>, msg_ptr: i32, msg_len: i32| -> i64 {
                    // Rate limit check.
                    if caller.data().sign_count >= SIGN_RATE_LIMIT {
                        tracing::warn!("craft_sign: rate limit exceeded");
                        return -1;
                    }

                    let mem = match caller.get_export("memory").and_then(|e| e.into_memory()) {
                        Some(m) => m,
                        None => return -3,
                    };
                    let data = mem.data(&caller);
                    let start = msg_ptr as usize;
                    let end = start.saturating_add(msg_len as usize);
                    if end > data.len() {
                        return -2;
                    }
                    let msg = data[start..end].to_vec();

                    let sig = caller.data().keystore.sign(&msg);
                    caller.data_mut().sign_count += 1;
                    caller.data_mut().scratch = sig.to_bytes().to_vec();

                    tracing::debug!(
                        msg_len = msg.len(),
                        count = caller.data().sign_count,
                        "craft_sign: success"
                    );
                    64
                },
            )
            .map_err(|e| ComError::RuntimeConfigError(e.to_string()))?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // craft_read_result
    // -----------------------------------------------------------------------

    /// `craft_read_result(dst_ptr: i32, offset: i32, len: i32) -> i32`
    ///
    /// Copy `len` bytes from the host scratch buffer (starting at `offset`)
    /// into the WASM module's linear memory at `dst_ptr`.
    ///
    /// ## Return value
    /// Number of bytes copied, or negative error code.
    fn register_read_result(linker: &mut Linker<HostState>) -> Result<(), ComError> {
        linker
            .func_wrap(
                HOST_MODULE,
                "craft_read_result",
                |mut caller: Caller<'_, HostState>, dst_ptr: i32, offset: i32, len: i32| -> i32 {
                    let off = offset as usize;
                    let n = len as usize;
                    let scratch_len = caller.data().scratch.len();

                    if off + n > scratch_len {
                        return -1;
                    }

                    let chunk = caller.data().scratch[off..off + n].to_vec();

                    let mem = match caller.get_export("memory").and_then(|e| e.into_memory()) {
                        Some(m) => m,
                        None => return -2,
                    };
                    let dst = dst_ptr as usize;
                    let mem_data = mem.data_mut(&mut caller);
                    if dst + n > mem_data.len() {
                        return -3;
                    }
                    mem_data[dst..dst + n].copy_from_slice(&chunk);
                    n as i32
                },
            )
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
    fn register_log(linker: &mut Linker<HostState>) -> Result<(), ComError> {
        linker
            .func_wrap(
                HOST_MODULE,
                "craft_log",
                |mut caller: Caller<'_, HostState>, level: i32, msg_ptr: i32, msg_len: i32| {
                    // Read message bytes from WASM linear memory.
                    let msg = if let Some(mem) =
                        caller.get_export("memory").and_then(|e| e.into_memory())
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
                        1 => tracing::warn!(target: "wasm_agent", "{}", msg),
                        2 => tracing::info!(target: "wasm_agent", "{}", msg),
                        3 => tracing::debug!(target: "wasm_agent", "{}", msg),
                        _ => tracing::trace!(target: "wasm_agent", "{}", msg),
                    }
                },
            )
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

    fn make_linker() -> Linker<HostState> {
        let engine = Engine::default();
        Linker::new(&engine)
    }

    #[test]
    fn all_host_functions_register_without_error() {
        let mut linker = make_linker();
        HostFunctions::register(&mut linker).expect("host function registration should succeed");
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
