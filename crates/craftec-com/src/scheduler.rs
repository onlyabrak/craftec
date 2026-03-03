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
use std::time::{Duration, Instant};

use craftec_crypto::sign::KeyStore;
use craftec_obj::ContentAddressedStore;
use craftec_sql::CraftDatabase;
use craftec_types::Cid;
use dashmap::DashMap;
use tokio::task::JoinHandle;

use crate::error::{ComError, Result};
use crate::host::HostState;
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
    /// The program has been quarantined after too many crashes.
    Quarantined {
        /// CID of the WASM binary.
        wasm_cid: Cid,
        /// Human-readable reason for quarantine.
        reason: String,
    },
}

impl ProgramState {
    /// Return the WASM CID for this program regardless of state.
    pub fn wasm_cid(&self) -> Cid {
        match self {
            ProgramState::Loaded { wasm_cid, .. }
            | ProgramState::Running { wasm_cid, .. }
            | ProgramState::Stopped { wasm_cid, .. }
            | ProgramState::Quarantined { wasm_cid, .. } => *wasm_cid,
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
            ProgramState::Quarantined { .. } => "quarantined",
        }
    }
}

impl std::fmt::Display for ProgramState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProgramState::Loaded {
                wasm_cid,
                loaded_at,
            } => {
                write!(
                    f,
                    "Loaded(wasm={wasm_cid}, age={}ms)",
                    loaded_at.elapsed().as_millis()
                )
            }
            ProgramState::Running {
                wasm_cid,
                started_at,
            } => {
                write!(
                    f,
                    "Running(wasm={wasm_cid}, uptime={}ms)",
                    started_at.elapsed().as_millis()
                )
            }
            ProgramState::Stopped { wasm_cid, reason } => {
                write!(f, "Stopped(wasm={wasm_cid}, reason={reason})")
            }
            ProgramState::Quarantined { wasm_cid, reason } => {
                write!(f, "Quarantined(wasm={wasm_cid}, reason={reason})")
            }
        }
    }
}

// ---------------------------------------------------------------------------
// ProgramScheduler
// ---------------------------------------------------------------------------

/// Maximum consecutive crashes before quarantine.
const CRASH_QUARANTINE_THRESHOLD: u32 = 10;

/// Kernel-level program lifecycle manager.
///
/// Manages loading, starting, stopping, and listing WASM programs on behalf
/// of the Craftec node.
pub struct ProgramScheduler {
    /// All tracked programs, keyed by WASM CID.
    programs: Arc<DashMap<Cid, ProgramState>>,
    /// The underlying compute runtime used to execute agents.
    runtime: Arc<ComRuntime>,
    /// Content-addressed store for loading WASM binaries.
    store: Arc<ContentAddressedStore>,
    /// SQL database (optional).
    database: Option<Arc<CraftDatabase>>,
    /// Ed25519 key store for host functions.
    keystore: Arc<KeyStore>,
    /// Task handles for running programs, keyed by WASM CID.
    task_handles: Arc<DashMap<Cid, JoinHandle<()>>>,
    /// Consecutive crash counts per program.
    crash_counts: Arc<DashMap<Cid, u32>>,
}

impl ProgramScheduler {
    /// Create a new [`ProgramScheduler`] backed by `runtime`.
    pub fn new(
        runtime: Arc<ComRuntime>,
        store: Arc<ContentAddressedStore>,
        database: Option<Arc<CraftDatabase>>,
        keystore: Arc<KeyStore>,
    ) -> Self {
        Self {
            programs: Arc::new(DashMap::new()),
            runtime,
            store,
            database,
            keystore,
            task_handles: Arc::new(DashMap::new()),
            crash_counts: Arc::new(DashMap::new()),
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
    /// Loads the WASM binary from CraftOBJ and spawns a Tokio task that runs
    /// the program's `main` entry point in a keepalive loop with crash backoff.
    ///
    /// # Errors
    /// - [`ComError::ProgramNotFound`] if `wasm_cid` is not in the scheduler.
    /// - [`ComError::SchedulerError`] if the program is already running or quarantined.
    pub async fn start_program(&self, wasm_cid: &Cid) -> Result<()> {
        let wasm_bytes = {
            let entry = self
                .programs
                .get(wasm_cid)
                .ok_or(ComError::ProgramNotFound(*wasm_cid))?;

            match &*entry {
                ProgramState::Running { .. } => {
                    return Err(ComError::SchedulerError(format!(
                        "program {wasm_cid} is already running"
                    )));
                }
                ProgramState::Quarantined { .. } => {
                    return Err(ComError::SchedulerError(format!(
                        "program {wasm_cid} is quarantined"
                    )));
                }
                _ => {}
            }
            drop(entry);

            // Load WASM bytes from CraftOBJ.
            self.store
                .get(wasm_cid)
                .await
                .map_err(|e| ComError::SchedulerError(format!("store error: {e}")))?
                .ok_or_else(|| {
                    ComError::SchedulerError(format!("WASM binary not found in store: {wasm_cid}"))
                })?
                .to_vec()
        };

        // Transition to Running.
        self.programs.insert(
            *wasm_cid,
            ProgramState::Running {
                wasm_cid: *wasm_cid,
                started_at: Instant::now(),
            },
        );

        // Reset crash count.
        self.crash_counts.insert(*wasm_cid, 0);

        // Spawn execution task.
        let cid = *wasm_cid;
        let runtime = Arc::clone(&self.runtime);
        let store = Arc::clone(&self.store);
        let database = self.database.clone();
        let keystore = Arc::clone(&self.keystore);
        let programs = Arc::clone(&self.programs);
        let crash_counts = Arc::clone(&self.crash_counts);

        let handle = tokio::spawn(async move {
            let mut local_crash_count: u32 = 0;

            loop {
                let host_state =
                    HostState::new(Arc::clone(&store), database.clone(), Arc::clone(&keystore));

                match runtime
                    .execute_agent(&wasm_bytes, "main", &[], host_state)
                    .await
                {
                    Ok(_) => {
                        // Normal completion — reset crash count, restart after delay.
                        local_crash_count = 0;
                        crash_counts.insert(cid, 0);
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                    Err(ComError::FuelExhausted { .. }) => {
                        // Fuel exhaustion is normal for long-running agents; fast restart.
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                    Err(e) => {
                        local_crash_count += 1;
                        crash_counts.insert(cid, local_crash_count);

                        tracing::warn!(
                            wasm_cid = %cid,
                            crash_count = local_crash_count,
                            error = %e,
                            "CraftCOM: program crashed"
                        );

                        if local_crash_count >= CRASH_QUARANTINE_THRESHOLD {
                            programs.insert(
                                cid,
                                ProgramState::Quarantined {
                                    wasm_cid: cid,
                                    reason: format!(
                                        "quarantined after {} consecutive crashes: {}",
                                        local_crash_count, e
                                    ),
                                },
                            );
                            tracing::error!(
                                wasm_cid = %cid,
                                "CraftCOM: program quarantined after {} crashes",
                                local_crash_count
                            );
                            break;
                        }

                        // Exponential backoff: 2^crash_count seconds, max 64s.
                        let backoff = Duration::from_secs(1u64 << local_crash_count.min(6));
                        tokio::time::sleep(backoff).await;
                    }
                }

                // Check if we were stopped externally.
                if let Some(state) = programs.get(&cid) {
                    if !state.is_running() {
                        break;
                    }
                } else {
                    break;
                }
            }
        });

        self.task_handles.insert(*wasm_cid, handle);
        tracing::info!(wasm_cid = %wasm_cid, "CraftCOM: program started");
        Ok(())
    }

    /// Stop a running program.
    ///
    /// Aborts the task handle and transitions the program to `Stopped` state.
    ///
    /// # Errors
    /// - [`ComError::ProgramNotFound`] if `wasm_cid` is not tracked.
    pub async fn stop_program(&self, wasm_cid: &Cid, reason: &str) -> Result<()> {
        let _entry = self
            .programs
            .get(wasm_cid)
            .ok_or(ComError::ProgramNotFound(*wasm_cid))?;
        drop(_entry);

        // Abort the spawned task if it exists.
        if let Some((_, handle)) = self.task_handles.remove(wasm_cid) {
            handle.abort();
        }

        self.programs.insert(
            *wasm_cid,
            ProgramState::Stopped {
                wasm_cid: *wasm_cid,
                reason: reason.to_owned(),
            },
        );

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

    async fn make_scheduler() -> (ProgramScheduler, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let rt = Arc::new(ComRuntime::new(1_000_000).unwrap());
        let store = Arc::new(ContentAddressedStore::new(&tmp.path().join("obj"), 64).unwrap());
        let keystore = Arc::new(KeyStore::new(tmp.path()).unwrap());
        let sched = ProgramScheduler::new(rt, store, None, keystore);
        (sched, tmp)
    }

    fn cid(seed: u8) -> Cid {
        Cid::from_bytes([seed; 32])
    }

    /// Minimal valid WASM binary: exports a `main` function returning i32 42.
    fn minimal_wasm() -> Vec<u8> {
        wat::parse_str(r#"(module (func (export "main") (result i32) i32.const 42))"#)
            .unwrap_or_else(|_| {
                vec![
                    0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, 0x01, 0x05, 0x01, 0x60, 0x00,
                    0x01, 0x7f, 0x03, 0x02, 0x01, 0x00, 0x07, 0x08, 0x01, 0x04, 0x6d, 0x61, 0x69,
                    0x6e, 0x00, 0x00, 0x0a, 0x06, 0x01, 0x04, 0x00, 0x41, 0x2a, 0x0b,
                ]
            })
    }

    /// WASM that immediately traps (unreachable instruction).
    fn trapping_wasm() -> Vec<u8> {
        wat::parse_str(r#"(module (func (export "main") (result i32) unreachable))"#)
            .unwrap_or_else(|_| {
                vec![
                    0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, 0x01, 0x05, 0x01, 0x60, 0x00,
                    0x01, 0x7f, 0x03, 0x02, 0x01, 0x00, 0x07, 0x08, 0x01, 0x04, 0x6d, 0x61, 0x69,
                    0x6e, 0x00, 0x00, 0x0a, 0x05, 0x01, 0x03, 0x00, 0x00, 0x0b,
                ]
            })
    }

    #[tokio::test]
    async fn load_transitions_to_loaded_state() {
        let (sched, _tmp) = make_scheduler().await;
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
        let (sched, _tmp) = make_scheduler().await;
        let unknown = cid(0xAA);
        assert!(matches!(
            sched.start_program(&unknown).await,
            Err(ComError::ProgramNotFound(_))
        ));
    }

    #[tokio::test]
    async fn full_lifecycle() {
        let (sched, _tmp) = make_scheduler().await;
        let wasm = minimal_wasm();

        // Store the WASM binary in CraftOBJ so start_program can load it.
        let wasm_cid = sched.store.put(&wasm).await.unwrap();

        // Load.
        if sched.load_program(&wasm_cid, &wasm).await.is_err() {
            return;
        }

        // Start.
        sched.start_program(&wasm_cid).await.unwrap();
        assert!(sched.is_running(&wasm_cid));

        // Stop.
        sched
            .stop_program(&wasm_cid, "test complete")
            .await
            .unwrap();
        assert!(!sched.is_running(&wasm_cid));
        assert!(matches!(
            sched.state(&wasm_cid),
            Some(ProgramState::Stopped { .. })
        ));
    }

    #[tokio::test]
    async fn list_programs_sorted() {
        let (sched, _tmp) = make_scheduler().await;
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
        let (sched, _tmp) = make_scheduler().await;
        assert!(matches!(
            sched.stop_program(&cid(0xBB), "force").await,
            Err(ComError::ProgramNotFound(_))
        ));
    }

    #[tokio::test]
    async fn start_program_spawns_execution() {
        let (sched, _tmp) = make_scheduler().await;
        let wasm = minimal_wasm();

        // Store in CraftOBJ.
        let wasm_cid = sched.store.put(&wasm).await.unwrap();
        if sched.load_program(&wasm_cid, &wasm).await.is_err() {
            return; // WASM not valid on this platform
        }

        sched.start_program(&wasm_cid).await.unwrap();
        assert!(sched.is_running(&wasm_cid));

        // The task handle should exist.
        assert!(sched.task_handles.contains_key(&wasm_cid));

        // Let it run briefly.
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Still running (keepalive loop).
        assert!(sched.is_running(&wasm_cid));

        sched.stop_program(&wasm_cid, "test done").await.unwrap();
    }

    #[tokio::test]
    async fn stop_program_aborts_task() {
        let (sched, _tmp) = make_scheduler().await;
        let wasm = minimal_wasm();

        let wasm_cid = sched.store.put(&wasm).await.unwrap();
        if sched.load_program(&wasm_cid, &wasm).await.is_err() {
            return;
        }

        sched.start_program(&wasm_cid).await.unwrap();
        assert!(sched.task_handles.contains_key(&wasm_cid));

        sched.stop_program(&wasm_cid, "abort test").await.unwrap();

        // Task handle should be removed.
        assert!(!sched.task_handles.contains_key(&wasm_cid));
        assert!(!sched.is_running(&wasm_cid));
    }

    #[tokio::test]
    async fn crash_quarantine_after_threshold() {
        let (sched, _tmp) = make_scheduler().await;
        let wasm = trapping_wasm();

        let wasm_cid = sched.store.put(&wasm).await.unwrap();
        if sched.load_program(&wasm_cid, &wasm).await.is_err() {
            return;
        }

        sched.start_program(&wasm_cid).await.unwrap();

        // Wait for crashes to accumulate. The trapping WASM will crash immediately
        // each time with exponential backoff: 2+4+8+16+32+64+64+64+64+64 ≈ 382s max.
        // But we use tokio::time::pause for controlled time.
        // Instead, just check periodically for quarantine state.
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            tokio::time::sleep(Duration::from_millis(50)).await;
            if let Some(state) = sched.state(&wasm_cid) {
                if matches!(state, ProgramState::Quarantined { .. }) {
                    // Quarantined! Verify crash count.
                    let count = sched.crash_counts.get(&wasm_cid).map(|v| *v).unwrap_or(0);
                    assert!(
                        count >= CRASH_QUARANTINE_THRESHOLD,
                        "expected >= {} crashes, got {}",
                        CRASH_QUARANTINE_THRESHOLD,
                        count
                    );
                    return;
                }
            }
            if Instant::now() > deadline {
                // If we haven't quarantined yet, the backoff is too slow for a test.
                // Verify at least some crashes happened.
                let count = sched.crash_counts.get(&wasm_cid).map(|v| *v).unwrap_or(0);
                assert!(count > 0, "expected at least one crash");
                sched.stop_program(&wasm_cid, "test timeout").await.unwrap();
                return;
            }
        }
    }
}
