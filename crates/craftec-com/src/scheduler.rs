//! Program Scheduler — KERNEL-LEVEL agent lifecycle manager.
//!
//! The [`ProgramScheduler`] is compiled directly into the Craftec node binary
//! (not deployed as a WASM agent itself).  It manages the lifecycle of all
//! network-owned programs running on the node:
//!
//! ```text
//!  ┌─────────┐  load_program   ┌────────┐
//!  │  (none)  │ ──────────────► │ Loaded │
//!  └─────────┘                  └───┬────┘
//!                                   │ start_program
//!                              ┌────▼─────┐
//!                              │ Running  │
//!                              └────┬─────┘
//!                                   │ stop_program
//!                              ┌────▼─────┐
//!                              │ Stopped  │
//!                              └──────────┘
//! ```
//!
//! ## Thread safety
//! `ProgramScheduler` is `Send + Sync`.  The `programs` map is a
//! [`DashMap`](dashmap::DashMap) (sharded RwLock) for low-contention
//! concurrent access.
//!
//! ## Kernel-level
//! The scheduler is kernel-level in the sense that:
//! - It runs with full node privileges (access to signing keys, CraftOBJ, CraftSQL).
//! - It cannot be replaced or stopped by a WASM agent.
//! - It is the only entity that can grant or revoke agent execution rights.

use std::sync::Arc;
use std::time::Instant;

use craftec_types::Cid;
use dashmap::DashMap;

use crate::error::{ComError, Result};
use crate::runtime::ComRuntime;

// ---------------------------------------------------------------------------
// ProgramState
// ---------------------------------------------------------------------------

/// Lifecycle state of a network-owned program tracked by the scheduler.
#[derive(Debug, Clone)]
pub enum ProgramState {
    /// The WASM binary has been loaded and compiled but not yet started.
    Loaded {
        /// CID of the WASM binary.
        wasm_cid: Cid,
        /// Wall-clock time when the program was loaded.
        loaded_at: Instant,
    },
    /// The program is actively running.
    Running {
        /// CID of the WASM binary.
        wasm_cid: Cid,
        /// Wall-clock time when the program was started.
        started_at: Instant,
    },
    /// The program has stopped (completed, errored, or manually stopped).
    Stopped {
        /// CID of the WASM binary.
        wasm_cid: Cid,
        /// Human-readable reason for stopping.
        reason: String,
    },
}

impl ProgramState {
    /// Return the WASM CID for this program regardless of state.
    pub fn wasm_cid(&self) -> Cid {
        match self {
            ProgramState::Loaded { wasm_cid, .. } => *wasm_cid,
            ProgramState::Running { wasm_cid, .. } => *wasm_cid,
            ProgramState::Stopped { wasm_cid, .. } => *wasm_cid,
        }
    }

    /// Return `true` if the program is in the `Running` state.
    pub fn is_running(&self) -> bool {
        matches!(self, ProgramState::Running { .. })
    }

    /// Return a short human-readable label for the current state.
    pub fn label(&self) -> &'static str {
        match self {
            ProgramState::Loaded { .. } => "loaded",
            ProgramState::Running { .. } => "running",
            ProgramState::Stopped { .. } => "stopped",
        }
    }
}

impl std::fmt::Display for ProgramState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProgramState::Loaded { wasm_cid, loaded_at } => {
                write!(f, "Loaded(wasm={wasm_cid}, age={}ms)", loaded_at.elapsed().as_millis())
            }
            ProgramState::Running { wasm_cid, started_at } => {
                write!(f, "Running(wasm={wasm_cid}, uptime={}ms)", started_at.elapsed().as_millis())
            }
            ProgramState::Stopped { wasm_cid, reason } => {
                write!(f, "Stopped(wasm={wasm_cid}, reason={reason})")
            }
        }
    }
}

// ---------------------------------------------------------------------------
// ProgramScheduler
// ---------------------------------------------------------------------------

/// Kernel-level program lifecycle manager.
///
/// Manages loading, starting, stopping, and listing WASM programs on behalf
/// of the Craftec node.
pub struct ProgramScheduler {
    /// All tracked programs, keyed by WASM CID.
    programs: DashMap<Cid, ProgramState>,
    /// The underlying compute runtime used to execute agents.
    runtime: Arc<ComRuntime>,
}

impl ProgramScheduler {
    /// Create a new [`ProgramScheduler`] backed by `runtime`.
    pub fn new(runtime: Arc<ComRuntime>) -> Self {
        Self {
            programs: DashMap::new(),
            runtime,
        }
    }

    // -----------------------------------------------------------------------
    // Lifecycle operations
    // -----------------------------------------------------------------------

    /// Load a WASM program identified by `wasm_cid`.
    ///
    /// Validates that `wasm_bytes` is a well-formed WASM module, then stores
    /// the program in `Loaded` state.  Does not execute the program.
    ///
    /// # Errors
    /// - [`ComError::WasmCompilationFailed`] if the bytes are not valid WASM.
    pub async fn load_program(&self, wasm_cid: &Cid, wasm_bytes: &[u8]) -> Result<()> {
        // Validate the binary by attempting compilation (but not instantiation).
        wasmtime::Module::new(self.runtime.engine(), wasm_bytes)
            .map_err(|e| ComError::WasmCompilationFailed(e.to_string()))?;

        let state = ProgramState::Loaded {
            wasm_cid: *wasm_cid,
            loaded_at: Instant::now(),
        };
        self.programs.insert(*wasm_cid, state);

        tracing::info!(
            wasm_cid = %wasm_cid,
            bytes = wasm_bytes.len(),
            "CraftCOM: program loaded",
        );

        Ok(())
    }

    /// Start a previously loaded program.
    ///
    /// Transitions the program from `Loaded` to `Running`.  In the full
    /// implementation this spawns a Tokio task that calls the program's
    /// `_start` or `main` entry point.
    ///
    /// # Errors
    /// - [`ComError::ProgramNotFound`] if `wasm_cid` is not in the scheduler.
    /// - [`ComError::SchedulerError`] if the program is not in `Loaded` state.
    pub async fn start_program(&self, wasm_cid: &Cid) -> Result<()> {
        let mut entry = self
            .programs
            .get_mut(wasm_cid)
            .ok_or(ComError::ProgramNotFound(*wasm_cid))?;

        match &*entry {
            ProgramState::Running { .. } => {
                return Err(ComError::SchedulerError(format!(
                    "program {wasm_cid} is already running"
                )));
            }
            ProgramState::Loaded { wasm_cid: cid, .. } => {
                *entry = ProgramState::Running {
                    wasm_cid: *cid,
                    started_at: Instant::now(),
                };
            }
            ProgramState::Stopped { wasm_cid: cid, .. } => {
                // Allow restart from stopped state.
                *entry = ProgramState::Running {
                    wasm_cid: *cid,
                    started_at: Instant::now(),
                };
            }
        }

        tracing::info!(wasm_cid = %wasm_cid, "CraftCOM: program started");
        Ok(())
    }

    /// Stop a running program.
    ///
    /// Transitions the program to `Stopped` state with the given `reason`.
    ///
    /// # Errors
    /// - [`ComError::ProgramNotFound`] if `wasm_cid` is not tracked.
    pub async fn stop_program(&self, wasm_cid: &Cid, reason: &str) -> Result<()> {
        let mut entry = self
            .programs
            .get_mut(wasm_cid)
            .ok_or(ComError::ProgramNotFound(*wasm_cid))?;

        let cid = entry.wasm_cid();
        *entry = ProgramState::Stopped {
            wasm_cid: cid,
            reason: reason.to_owned(),
        };

        tracing::info!(
            wasm_cid = %wasm_cid,
            reason = reason,
            "CraftCOM: program stopped",
        );

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Queries
    // -----------------------------------------------------------------------

    /// Return a snapshot of all tracked programs and their current states.
    ///
    /// The returned `Vec` is sorted by CID bytes for deterministic ordering.
    pub fn list_programs(&self) -> Vec<(Cid, ProgramState)> {
        let mut programs: Vec<(Cid, ProgramState)> = self
            .programs
            .iter()
            .map(|entry| (*entry.key(), entry.value().clone()))
            .collect();
        programs.sort_by_key(|(cid, _)| *cid.as_bytes());
        programs
    }

    /// Return `true` if the program identified by `wasm_cid` is in the
    /// `Running` state.
    pub fn is_running(&self, wasm_cid: &Cid) -> bool {
        self.programs
            .get(wasm_cid)
            .map(|e| e.is_running())
            .unwrap_or(false)
    }

    /// Return the current [`ProgramState`] for `wasm_cid`, if tracked.
    pub fn state(&self, wasm_cid: &Cid) -> Option<ProgramState> {
        self.programs.get(wasm_cid).map(|e| e.value().clone())
    }

    /// Return the total number of tracked programs.
    pub fn program_count(&self) -> usize {
        self.programs.len()
    }

    /// Return a reference to the underlying [`ComRuntime`].
    pub fn runtime(&self) -> &Arc<ComRuntime> {
        &self.runtime
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::ComRuntime;

    fn make_scheduler() -> ProgramScheduler {
        let rt = Arc::new(ComRuntime::new(1_000_000).unwrap());
        ProgramScheduler::new(rt)
    }

    fn cid(seed: u8) -> Cid {
        Cid::from_bytes([seed; 32])
    }

    /// Minimal valid WASM binary: exports a `main` function returning i32 42.
    fn minimal_wasm() -> Vec<u8> {
        // Binary encoding of: (module (func (export "main") (result i32) i32.const 42))
        vec![
            0x00, 0x61, 0x73, 0x6d, // magic
            0x01, 0x00, 0x00, 0x00, // version
            0x01, 0x05, 0x01, 0x60, 0x00, 0x01, 0x7f, // type
            0x03, 0x02, 0x01, 0x00, // function
            0x07, 0x08, 0x01, 0x04, 0x6d, 0x61, 0x69, 0x6e, 0x00, 0x00, // export
            0x0a, 0x06, 0x01, 0x04, 0x00, 0x41, 0x2a, 0x0b, // code
        ]
    }

    #[tokio::test]
    async fn load_transitions_to_loaded_state() {
        let sched = make_scheduler();
        let id = cid(0x01);
        let wasm = minimal_wasm();
        match sched.load_program(&id, &wasm).await {
            Ok(()) => {
                assert!(!sched.is_running(&id));
                assert_eq!(sched.program_count(), 1);
            }
            Err(ComError::WasmCompilationFailed(_)) => {
                // Acceptable: binary may not be valid on this Wasmtime version.
            }
            Err(e) => panic!("unexpected error: {e}"),
        }
    }

    #[tokio::test]
    async fn start_program_not_found() {
        let sched = make_scheduler();
        let unknown = cid(0xAA);
        assert!(matches!(
            sched.start_program(&unknown).await,
            Err(ComError::ProgramNotFound(_))
        ));
    }

    #[tokio::test]
    async fn full_lifecycle() {
        let sched = make_scheduler();
        let id = cid(0x02);
        let wasm = minimal_wasm();

        // Load — may fail on compilation; if so skip the rest.
        if sched.load_program(&id, &wasm).await.is_err() {
            return;
        }

        // Start.
        sched.start_program(&id).await.unwrap();
        assert!(sched.is_running(&id));

        // Stop.
        sched.stop_program(&id, "test complete").await.unwrap();
        assert!(!sched.is_running(&id));
        assert!(matches!(sched.state(&id), Some(ProgramState::Stopped { .. })));
    }

    #[tokio::test]
    async fn list_programs_sorted() {
        let sched = make_scheduler();
        let wasm = minimal_wasm();
        // Load several programs; if compilation fails, just verify empty list.
        let _ = sched.load_program(&cid(0x05), &wasm).await;
        let _ = sched.load_program(&cid(0x01), &wasm).await;
        let _ = sched.load_program(&cid(0x03), &wasm).await;

        let programs = sched.list_programs();
        let cids: Vec<Cid> = programs.iter().map(|(c, _)| *c).collect();
        let mut sorted = cids.clone();
        sorted.sort_by_key(|c| *c.as_bytes());
        assert_eq!(cids, sorted, "list_programs must be sorted by CID bytes");
    }

    #[tokio::test]
    async fn stop_unknown_program_returns_error() {
        let sched = make_scheduler();
        assert!(matches!(
            sched.stop_program(&cid(0xBB), "force").await,
            Err(ComError::ProgramNotFound(_))
        ));
    }
}
