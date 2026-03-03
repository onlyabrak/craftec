# Craftec Deep Lifecycle Audit — Commit `1c0afe2`

**Date**: 2026-03-03
**Previous audit**: Commit `dd33869` (T1–T15, G1–G5)
**Scope**: Full lifecycle trace of all fixes, sequence/order/timing correctness, verbose logging plan, Docker multi-node test plan

---

## Part A — Verification of All Audit Fixes (T1–T15, G1–G5)

### T1: HLC — Hybrid Logical Clock ✅ FIXED

**File**: `craftec-types/src/hlc.rs`

- 64-bit packed layout: `[48-bit wall_ms | 16-bit logical]`
- `now()` uses CAS loop (`compare_exchange_weak` with `Ordering::AcqRel`) — lock-free, monotonic
- `observe()` enforces 500ms max skew (`MAX_SKEW_MS`), ±30s replay window (`REPLAY_WINDOW_MS`)
- Logical counter overflow handled: when `logical == u16::MAX`, forces `wall_ms + 1`
- `HlcError::ClockSkew` and `HlcError::ReplayDetected` are distinct error types with diagnostic fields
- Tests: monotonic (1000 iterations), observe advances, skew rejection, replay detection, pack/unpack roundtrip

**Audit verdict**: Correct. No remaining issues.

### T2: SWIM Probe-Ack Correlation ✅ FIXED (revised)

**File**: `craftec-net/src/swim.rs`

- `probe_nonce: AtomicU64` — monotonic counter for unique nonces
- `pending_probes: DashMap<u64, oneshot::Sender<u64>>` — correlates nonce to ack
- `resolve_probe(nonce, incarnation) → bool` — removes from map and sends incarnation
- `random_alive_excluding()` — selects K=3 indirect delegates for ping-req path
- `SwimPing` carries `nonce`; `SwimPingAck` echoes `nonce + incarnation`

**Original audit missed a bug**: `protocol_tick()` allocated nonce N and baked it into the `SwimPing` message, then `probe_with_ack()` called `register_probe()` which allocated a *second* nonce N+1. The remote peer responded with nonce N, but the pending probe was registered under N+1 — so `resolve_probe(N)` never matched. All direct probes timed out, causing spurious suspect/dead transitions and a slow memory leak in `pending_probes`.

**Fix** (commit `ae90a79`): `probe_with_ack()` now extracts the nonce from the already-built `SwimPing` message and registers a probe receiver under that nonce, instead of calling `register_probe()`. Also fixed: indirect probes were sending to `target` instead of delegates (the `_delegate` variable was unused).

**Audit verdict**: Correct after fix. Nonce-correlated probes prevent ack misrouting.

### T3: PendingFetches Timeout & Staleness ✅ FIXED

**File**: `craftec-net/src/pending.rs`

- `PendingEntry { tx, registered_at: Instant }` — timestamps each registration
- `prune_stale(max_age: Duration)` — removes entries where `age >= max_age` OR `tx.is_closed()`
- After pruning, empty CID entries are cleaned up via `retain()`
- Tests: prune closed receivers, keep active, total pending tracking

**Audit verdict**: Correct. Stale entries are properly cleaned.

### T4: See G2 (Event Bus Wiring)

### T5: Wire Frame Header v1 ✅ FIXED

**File**: `craftec-types/src/wire.rs`

- `FRAME_HEADER_SIZE = 17` bytes: `[type_tag:4 | version:1 | hlc_ts:8 | payload_len:4]`
- `WIRE_VERSION = 1`
- `encode_framed_with_hlc(msg, hlc_ts)` — writes v1 header
- `decode_framed_with_hlc(data) → (WireMessage, u64)` — reads v1 with HLC
- V0 backward compat: `FRAME_HEADER_V0_SIZE = 9` — old frames decode with `hlc_ts = 0`
- Tests: all 13 variants framed roundtrip, wrong version fails, truncated fails, v0 compat, v1 with HLC

**Audit verdict**: Correct. Backward compatible with v0.

### T6/T7: SWIM Atomic State Transitions ✅ FIXED

**File**: `craftec-net/src/swim.rs`

- `mark_alive()` uses `entry().and_modify().or_insert()` — single atomic operation
- `mark_suspect()` uses `entry().and_modify().or_insert()` — single atomic operation
- `mark_dead()` uses `entry().and_modify().or_insert()` — single atomic operation
- Incarnation comparisons prevent stale updates:
  - Alive: rejects if `incarnation <= existing` (for Alive state) or `incarnation < existing` (for Suspect)
  - Suspect: only updates Alive if `incarnation >=`, only updates Suspect if `incarnation >`
  - Dead: only overrides existing Dead if `incarnation >`, always overrides Alive/Suspect
- `incarnation: AtomicU64` uses `Ordering::Acquire` for loads, `Ordering::AcqRel` for fetch_add

**Audit verdict**: Correct. No TOCTOU races. Entry API prevents read-then-write gaps.

### T8: Suspect Timeout ✅ FIXED

**File**: `craftec-net/src/swim.rs`

- `DEFAULT_SUSPECT_TIMEOUT = Duration::from_millis(5000)` — matches spec §18
- `mark_suspect()` records `since: Instant::now()` for timeout tracking
- SWIM loop checks `suspect_timeout` expiry and promotes to Dead

**Audit verdict**: Correct. 5000ms per spec.

### T9: SQL Mutex Hold-Through-Commit ✅ FIXED

**File**: `craftec-sql/src/database.rs`

- `execute()` method: `let conn = self.write_conn.lock().await;` acquired first (C2: now uses dedicated write connection)
- SQL execution: `conn.execute(sql, ()).await`
- VFS sync: `Self::sync_pages_to_vfs(&self.db_path, &self.vfs).await`
- Root update: `*self.root_cid.write() = new_root;`
- Event publish: PageCommitted event sent
- `drop(conn)` — explicit release AFTER all commit steps
- Comment: "T9 fix: hold the conn mutex through the entire execute-commit cycle"
- `query()` uses separate `read_conn` — reads don't block behind writes (C2 fix)

**Audit verdict**: Correct. The write_conn mutex serializes execute→sync→root_update→event atomically. Reads use independent read_conn.

### T10: Request/Response Correlation IDs ✅ FIXED

**Files**: `craftec-types/src/wire.rs`, `craftec-node/src/handler.rs`

- `PieceRequest { cid, piece_indices, request_id: u64 }` — carries correlation ID
- `PieceResponse { pieces, request_id: u64 }` — echoes correlation ID
- Handler: `PieceRequest` handler returns response with same `request_id`
- `RepairExecutor` uses `AtomicU64::fetch_add(1, Ordering::Relaxed)` for unique request IDs

**Audit verdict**: Correct. Correlation prevents mismatched responses.

### T11: Stream Read Timeouts ✅ FIXED

**File**: `craftec-health/src/repair.rs`

- `FETCH_TIMEOUT = Duration::from_secs(10)` — per-peer timeout
- `tokio::time::timeout(FETCH_TIMEOUT, rx).await` wraps every fetch response wait
- Timeout or channel close logs debug and continues to next holder

**Audit verdict**: Correct. No unbounded reads.

### T13: DHT Provider TTL ✅ FIXED

**File**: `craftec-net/src/dht.rs` (verified from diff)

- `ProviderRecord { node_id, announced_at: Instant }` — timestamps each announcement
- `prune_stale(ttl: Duration)` — removes records older than TTL
- TTL-based eviction prevents stale provider records

**Audit verdict**: Correct.

### T15: Graceful Shutdown via JoinSet ✅ FIXED

**File**: `craftec-node/src/node.rs`

- `let mut tasks = tokio::task::JoinSet::new();` — all background tasks collected
- Storage bootstrap, accept loop, SWIM loop, health scan, repair executor, event dispatch — all `tasks.spawn()`
- Shutdown sequence:
  1. `event_bus.publish(ShutdownSignal)` — notifies event subscribers
  2. `shutdown_tx.send(())` — broadcast to all tasks
  3. `tokio::time::timeout(Duration::from_secs(5), join_all)` — 5s grace
  4. If timeout: `tasks.abort_all()` — force abort
  5. Remove `node.lock` sentinel file

**Audit verdict**: Correct. No fixed `sleep(500ms)` — proper JoinSet with timeout.

### G1: File-Backed SQLite (CraftSQL) ✅ FIXED

**File**: `craftec-sql/src/database.rs`

- `CraftDatabase::create(owner, vfs, data_dir)` — now takes 3rd arg `data_dir: &Path`
- Creates `data_dir/craftec.db` as a real file-backed libsql database
- `PRAGMA page_size = 16384` and `PRAGMA journal_mode = DELETE` (no WAL per spec §35)
- `sync_pages_to_vfs()` reads actual SQLite pages from disk and writes to CID-VFS
- Initial `_craftec_meta` table created to force page generation

**Audit verdict**: Correct. No more virtual-only VFS — real SQLite pages backed by CID-VFS.

### G2: Event Bus Wiring (CraftOBJ + CraftSQL) ✅ FIXED

**Files**: `craftec-node/src/event_bus.rs`, `craftec-node/src/node.rs`, `craftec-obj/src/store.rs`

- `EventBus::sender()` — returns cloneable `broadcast::Sender<Event>`
- `node.rs` Step 8: `store.set_event_sender(event_bus.sender())` — OBJ publishes CidWritten
- `node.rs` Step 8: `database.set_event_sender(event_bus.sender())` — SQL publishes PageCommitted
- `ContentAddressedStore::put()` fires `CidWritten { cid }` only on actual writes (not dedup)
- `CraftDatabase::execute()` fires `PageCommitted { db_id, page_num, root_cid }`

**Audit verdict**: Correct. Both subsystems publish events via post-init injection.

### G3: RLNC Encode on Write ✅ FIXED (revised)

**File**: `craftec-node/src/node.rs` (event dispatch loop), `craftec-node/src/piece_store.rs`

- Event dispatch loop handles `CidWritten`:
  1. DHT announce: `announce_cid_to_peers()`
  2. Recursion guard: `if piece_index.is_piece_cid(&cid) { continue; }` — skip piece CIDs
  3. Fire-and-forget `tokio::spawn` (C1 fix):
     a. Fetch raw data: `store.get(&cid).await`
     b. RLNC encode: `rlnc.encode(&data, k=32).await`
     c. For each coded piece: pre-compute CID, mark as piece CID, THEN `store.put()`
     d. Record in piece tracker + piece index

- `CodedPieceIndex`:
  - `DashMap<Cid, Vec<Cid>>` — maps content CID → piece CIDs
  - `DashSet<Cid>` — tracks all piece CIDs for recursion prevention

**Original audit missed a race condition**: Step 6 (`mark_piece_cid`) ran AFTER `store.put()` (Step 5). When C1 moved encoding to `tokio::spawn`, the event loop could receive the `CidWritten` for a coded piece before `mark_piece_cid` was called, causing recursive RLNC encoding.

**Fix** (commit `ae90a79`): Pre-compute CID via `Cid::from_data(&bytes)` and call `mark_piece_cid()` BEFORE `store.put()` fires `CidWritten`. Also replaced `unwrap_or_default()` on `postcard::to_allocvec()` with proper error handling.

**Audit verdict**: Correct after fix. No infinite encoding loop.

### G4: See T10 + T11 (correlation + timeouts)

### G5: SWIM Incarnation ✅ FIXED

Covered under T6/T7. All incarnation operations use correct atomic ordering.

---

## Part B — Full Node Lifecycle Trace

### Phase 1: Init (`CraftecNode::new`)

```
Step  1  create/verify data_dir          → fs::create_dir_all
Step  2  write node.lock sentinel        → fs::write("locked") — dirty shutdown detection
Step  3  KeyStore(Ed25519)               → load or generate from data_dir/node.key
Step  4  CraftOBJ store                  → ContentAddressedStore::new(data_dir/obj, 1024)
                                            → 256 shard dirs, bloom filter rebuild
Step  5  RLNC engine                     → RlncEngine::new()
Step  6  CID-VFS                         → CidVfs::new(store, page_size)
Step  6b CraftSQL database               → CraftDatabase::create(node_id, vfs, data_dir/sql)
                                            → file-backed libsql, PRAGMA page_size/journal_mode
                                            → sync_pages_to_vfs → initial root CID
Step  7  CraftCOM runtime                → ComRuntime::new(fuel_limit=10M)
Step  8  Event bus                       → EventBus::new(capacity=1024)
                                            → store.set_event_sender(bus.sender())
                                            → database.set_event_sender(bus.sender())
Step  9  iroh endpoint                   → CraftecEndpoint::new(config, keypair)
                                            → QUIC listener bound
Step 10  SWIM + DHT + PendingFetches     → swim from endpoint, DhtProviders::new()
Step 11  HealthScanner + PieceTracker    → scanner(store, tracker, interval)
Step 11b CodedPieceIndex                 → DashMap + DashSet for RLNC tracking
Step 12  ProgramScheduler               → scheduler(runtime, store, Some(db), keystore)
         shutdown channel                → broadcast::channel(16)
```

**Ordering correctness**:
- Store (Step 4) created before VFS (Step 6) — VFS depends on store ✅
- VFS created before DB (Step 6b) — DB depends on VFS ✅
- Event bus (Step 8) created after store + DB — then injected via `set_event_sender` ✅
- Endpoint (Step 9) created after keystore — uses keypair ✅
- SWIM (Step 10) derived from endpoint — correct dependency ✅
- Health scanner (Step 11) uses store + tracker — both exist ✅

**Potential issue**: None. Init order is sound.

### Phase 2: Run (`CraftecNode::run`)

```
Bootstrap          → endpoint.bootstrap(peers) — connect to seeds
Storage bootstrap  → spawned task: list_cids → announce each to DHT
Accept loop        → spawned: endpoint.accept_loop(handler) ← inbound QUIC
SWIM loop          → spawned: run_swim_loop(swim, endpoint, shutdown_rx)
Health scan        → spawned: scanner.run(repair_tx, shutdown_rx)
Repair executor    → spawned: consumes repair_rx, calls execute_repair()
Event dispatch     → spawned: subscribes to event_bus, processes events
Wait               → wait_for_shutdown() — blocks on Ctrl+C/SIGTERM
```

**Ordering correctness**:
- Bootstrap happens before accept loop — ensures we know peers first ✅
- Storage bootstrap runs in parallel — announces local CIDs ✅
- Accept loop uses `NodeMessageHandler` with all subsystem refs — all constructed ✅
- SWIM and Health run independently — no ordering dependency ✅
- Event dispatch subscribes before any events could be published — correct ✅

### Phase 3: Store (Write Path)

```
App → CraftOBJ::put(data)
  → BLAKE3(data) → CID
  → bloom check (dedup fast path)
  → write to tmp file → rename atomically
  → bloom.insert + cache.put
  → event_bus: CidWritten { cid }

Event dispatch receives CidWritten:
  → DHT announce to alive peers
  → recursion guard: skip if piece_index.is_piece_cid(cid)
  → tokio::spawn fire-and-forget (C1 fix):
    → RLNC encode(data, k=32) → Vec<CodedPiece>
    → piece_tracker.record_k(cid, k)  ← C7 fix
    → for each piece:
        → postcard::to_allocvec(piece) (with error handling, skip on failure)
        → Cid::from_data(bytes) → pre-compute piece_cid
        → piece_index.mark_piece_cid(piece_cid)  ← BEFORE put (race fix)
        → store.put(bytes) → piece_cid (fires CidWritten, but already marked)
        → piece_tracker.record_piece(cid, holder)
    → piece_index.insert(cid, all_piece_cids)
```

**Ordering correctness**:
- CID computed before any I/O ✅
- Atomic write (tmp → rename) ensures no partial reads ✅
- Bloom + cache updated after successful write ✅
- Event only on actual writes (dedup path skips event) ✅
- Recursion guard checked BEFORE encoding ✅

**Previously noted**: RLNC encode ran inline in the event dispatch loop. **Fixed** (C1): Now runs in fire-and-forget `tokio::spawn`. Race condition in piece CID marking also fixed — CID pre-computed and marked BEFORE `store.put()` fires `CidWritten`.

### Phase 4: Query (Read Path)

```
App → CraftOBJ::get(cid)
  → cache check (LRU) — microsecond
  → bloom check — if negative, return None immediately
  → disk read (shard_path)
  → BLAKE3 integrity verification: actual_cid == requested_cid
  → cache.put for future reads
  → return bytes

App → CraftSQL::query(sql)
  → vfs.snapshot() — pins root CID
  → conn.lock().await — shared read (tokio Mutex)
  → conn.query(sql, ()) — libsql execution
  → parse rows → Vec<Row>
```

**Ordering correctness**:
- Bloom before disk — eliminates I/O for absent CIDs ✅
- Integrity check after every disk read — silent corruption impossible ✅
- SQL reads hold conn mutex — prevents interleave with writes ✅

**Previously noted**: SQL reads took the same `tokio::sync::Mutex` as writes. **Fixed** (C2): Separate `write_conn` and `read_conn` with `PRAGMA busy_timeout = 5000` on both.

### Phase 5: Network Message Handling

```
Inbound QUIC connection → accept_loop → handler.handle_message(from, msg)

  Ping → reply Pong (echo nonce)
  PieceRequest → look up piece_index → serve coded pieces OR identity fallback
  PieceResponse → pending_fetches.resolve(cid, piece) — wakes waiting tasks
  ProviderAnnounce → dht.announce_provider(cid, node_id)
  HealthReport → piece_tracker.record_piece() for each reported piece
  SignedWrite → verify signature → check ownership → CAS check → execute
  SwimPing/Ack → SWIM protocol handling
```

**Ordering correctness**:
- PendingFetches: register BEFORE send ensures no race ✅
- SignedWrite: signature → ownership → CAS → execute — correct order ✅
- HealthReport records `Instant::now()` for freshness ✅

### Phase 6: Health Scan & Repair

```
HealthScanner::run() loop:
  → sleep(interval / 100) — 36s per cycle at default
  → scan_cycle():
    → piece_tracker.sorted_cids()
    → batch = 1% of CIDs from cursor position
    → for each CID in batch:
        → available = tracker.available_count(cid)
        → if available < k(32): Critical repair
        → if available < target(ceil(2+16/k)): Normal repair
    → advance cursor (Acquire/Release ordering)

RepairExecutor::execute_repair():
  → fetch ≥2 pieces from holders (10s timeout each)
  → RLNC recode (NOT decode) — linear combination
  → select target: prefer nodes NOT already holding pieces
  → distribute recoded piece via PieceResponse
```

**Ordering correctness**:
- Cursor uses `Acquire` for load, `Release` for store — correct memory ordering ✅
- Register interest BEFORE sending fetch request — prevents race ✅
- Recode (not decode) — nodes never reconstruct original data ✅
- Target selection prefers diversity — increases redundancy spread ✅

### Phase 7: Shutdown

```
Ctrl+C / SIGTERM → wait_for_shutdown() returns
→ event_bus.publish(ShutdownSignal) — inform event subscribers
→ shutdown_tx.send(()) — broadcast to all tasks
→ timeout(5s, join all tasks)
  → if timeout: tasks.abort_all()
→ remove node.lock sentinel
→ return Ok(())
```

**Ordering correctness**:
- ShutdownSignal event published BEFORE broadcast — event loop can clean up ✅
- JoinSet with 5s timeout — bounded shutdown time ✅
- node.lock removed last — prevents false dirty-shutdown detection ✅

---

## Part C — Issues Found & Resolved

All 7 issues identified during the audit have been fixed in commit `ae90a79`.

### C1: RLNC Encoding Blocks Event Loop ✅ FIXED

**Location**: `craftec-node/src/node.rs`

**Was**: RLNC encoding ran inline in the event dispatch loop, blocking all event processing.

**Fix**: Moved to fire-and-forget `tokio::spawn`. DHT announce stays inline (lightweight). Pre-compute CID and `mark_piece_cid()` BEFORE `store.put()` to prevent race condition with recursive encoding. Serialization failures handled with `match` instead of `unwrap_or_default()`.

### C2: SQL Read Contention Under Write Load ✅ FIXED

**Location**: `craftec-sql/src/database.rs`

**Was**: Both `execute()` and `query()` held the same `tokio::sync::Mutex<libsql::Connection>`.

**Fix**: Split into `write_conn` and `read_conn`. `execute()` uses `write_conn`, `query()` uses `read_conn`. Both connections have `PRAGMA busy_timeout = 5000` to handle lock contention gracefully. New test: `concurrent_read_during_write`.

### C3: SWIM Piggyback Doesn't Scale ✅ FIXED

**Location**: `craftec-net/src/swim.rs`

**Was**: Piggyback gossip count hardcoded to 4 regardless of cluster size.

**Fix**: `adaptive_piggyback_count(n)` returns `max(4, ceil(log2(n)))`. Added `alive_count()` method and `tick_count` with periodic membership summaries every 10 ticks. Protocol period stays constant per SWIM paper. 3 new tests.

### C4: Storage Bootstrap Is Unbounded ✅ FIXED

**Location**: `craftec-node/src/node.rs`

**Was**: Listed ALL local CIDs and announced each one sequentially with no rate limiting.

**Fix**: Batched CID announcements (100 CIDs/batch) with 1s sleep between batches. Shutdown-aware via `tokio::select!` with `shutdown_rx.try_recv()` check between batches. Logs progress.

### C5: No Periodic PendingFetches Pruning ✅ FIXED

**Location**: `craftec-node/src/node.rs`

**Fix**: Background task spawned in `run()` with 60s interval, `prune_stale(Duration::from_secs(120))`. Shutdown-aware via `tokio::select!`. Logs pruned count at debug level.

### C6: DHT Provider Pruning Not Scheduled ✅ FIXED

**Location**: `craftec-node/src/node.rs`

**Fix**: Background task spawned in `run()` with 60s interval, `prune_stale(Duration::from_secs(300))` (5min TTL). Shutdown-aware via `tokio::select!`. Logs pruned count at debug level.

### C7: HealthScanner Uses Hardcoded K=32 ✅ FIXED

**Location**: `craftec-health/src/tracker.rs`, `craftec-health/src/scanner.rs`, `craftec-node/src/node.rs`

**Was**: `DEFAULT_K = 32` hardcoded in scanner.

**Fix**: Added `k_values: Arc<DashMap<Cid, u32>>` to `PieceTracker` with `record_k()` (first-writer-wins) and `get_k()`. Scanner uses `piece_tracker.get_k(cid).unwrap_or(DEFAULT_K)`. K values cleaned up in `remove_node()`. RLNC encode calls `record_k(&cid, k)` after encoding. 3 new tests.

---

## Part D — Verbose Structured Logging ✅ IMPLEMENTED

All logging additions below were implemented in commit `ae90a79`.

### Principles

1. All logs use `tracing` crate with structured fields (not format strings)
2. Log levels: `ERROR` = data loss risk, `WARN` = degraded, `INFO` = lifecycle milestones, `DEBUG` = per-operation, `TRACE` = wire-level
3. Every subsystem has a consistent prefix: `CraftOBJ:`, `CraftSQL:`, `SWIM:`, `RLNC:`, `Health:`, `CraftCOM:`, `Event:`
4. Every operation includes: `node_id`, `cid` (where applicable), `duration_ms`, `result`
5. Correlation IDs: `request_id` for piece exchange, `nonce` for SWIM probes, `db_id` for SQL ops

### Subsystem Logging Matrix

#### CraftOBJ (`craftec-obj/src/store.rs`)

| Event | Level | Fields | Current | Needed |
|-------|-------|--------|---------|--------|
| put start | DEBUG | cid, size | ✅ Yes | Add `source` (local/network) |
| put dedup | TRACE | cid | ✅ Yes | OK |
| put complete | TRACE | cid, size | ✅ Yes | Add `duration_ms` |
| get cache hit | DEBUG | cid | ✅ Yes | OK |
| get cache miss | DEBUG | cid | ✅ partial | Add `layer` (bloom/disk) |
| get integrity fail | ERROR | cid, actual_cid, path | ✅ Yes | Add `file_size`, `shard` |
| delete | DEBUG | cid | ✅ Yes | Add `freed_bytes` |
| bloom false positive | TRACE | cid | ✅ Yes | OK |
| event published | DEBUG | cid, event_type | ✅ implicit | Add explicit `event=CidWritten` |

**New logs needed**:
```rust
// On put completion:
tracing::debug!(cid = %cid, size = data.len(), duration_ms = start.elapsed().as_millis(), "CraftOBJ: put complete");

// On bloom miss:
tracing::trace!(cid = %cid, layer = "bloom", "CraftOBJ: get — bloom negative, skipping disk");

// On disk read:
tracing::debug!(cid = %cid, layer = "disk", size = data.len(), duration_ms = start.elapsed().as_millis(), "CraftOBJ: get — disk read");
```

#### CraftSQL (`craftec-sql/src/database.rs`)

| Event | Level | Fields | Current | Needed |
|-------|-------|--------|---------|--------|
| create | INFO | owner, db_id, path | ✅ Yes | OK |
| execute start | DEBUG | db_id, sql | ✅ Yes | Add `writer`, `expected_root` |
| execute commit | DEBUG | db_id, new_root | ✅ Yes | Add `duration_ms`, `pages_synced` |
| query start | DEBUG | - | ❌ No | Add `db_id`, `sql_preview` (first 100 chars) |
| query result | DEBUG | - | ❌ No | Add `row_count`, `duration_ms` |
| ownership reject | WARN | writer, owner | ❌ implicit | Add explicit log |
| CAS conflict | WARN | expected, actual | ❌ implicit | Add explicit log |
| RPC write | INFO | writer, new_root | ✅ Yes | OK |

**New logs needed**:
```rust
// Query start:
tracing::debug!(db_id = %self.db_id, sql_preview = &sql[..sql.len().min(100)], "CraftSQL: query start");

// Query complete:
tracing::debug!(db_id = %self.db_id, rows = rows.len(), duration_ms = start.elapsed().as_millis(), "CraftSQL: query complete");

// Ownership rejection:
tracing::warn!(writer = %ctx.writer, owner = %owner, "CraftSQL: write rejected — unauthorized writer");

// CAS conflict:
tracing::warn!(expected = %expected, actual = %actual, "CraftSQL: write rejected — CAS conflict");
```

#### CraftNet / SWIM (`craftec-net/src/swim.rs`)

| Event | Level | Fields | Current | Needed |
|-------|-------|--------|---------|--------|
| init | INFO | local_id | ✅ Yes | OK |
| mark_alive | DEBUG | node, incarnation | ✅ Yes | Add `previous_state` |
| mark_suspect | DEBUG | node, incarnation | ✅ Yes | Add `previous_state` |
| mark_dead | INFO | node, incarnation | ✅ Yes | OK |
| refute suspect | INFO | new_incarnation | ✅ Yes | OK |
| probe sent | DEBUG | target, nonce | ❌ No | Add |
| probe ack received | DEBUG | from, nonce, incarnation | ❌ No | Add |
| probe timeout | WARN | target, nonce, timeout_ms | ❌ No | Add |
| indirect probe | DEBUG | target, delegates | ❌ No | Add |
| membership count change | INFO | alive, suspect, dead, total | ❌ No | Add periodic summary |

**New logs needed**:
```rust
// Probe sent:
tracing::debug!(target = %target, nonce, "SWIM: probe sent");

// Probe ack:
tracing::debug!(from = %from, nonce, incarnation, "SWIM: probe ack received");

// Probe timeout → suspect:
tracing::warn!(target = %target, nonce, "SWIM: probe timeout — marking suspect");

// Periodic summary (every 10 ticks):
tracing::info!(alive = alive_count, suspect = suspect_count, dead = dead_count, total = total, "SWIM: membership summary");
```

#### RLNC (`craftec-rlnc/src/engine.rs`)

| Event | Level | Fields | Current | Needed |
|-------|-------|--------|---------|--------|
| encode start | DEBUG | - | ❌ No | Add `cid`, `data_size`, `k` |
| encode complete | DEBUG | - | ❌ No | Add `cid`, `pieces`, `duration_ms` |
| recode start | DEBUG | - | ❌ No | Add `input_pieces`, `cid` |
| recode complete | DEBUG | - | ❌ No | Add `cid`, `duration_ms` |
| decode start | DEBUG | - | ❌ No | Add `cid`, `pieces_available`, `k` |
| decode complete | DEBUG | - | ❌ No | Add `cid`, `data_size`, `duration_ms` |
| decode failure | ERROR | - | ❌ No | Add `cid`, `reason` |

#### Health Scanner (`craftec-health/src/scanner.rs`)

| Event | Level | Fields | Current | Needed |
|-------|-------|--------|---------|--------|
| init | INFO | interval | ✅ Yes | OK |
| cycle start | TRACE | cursor, batch_size, total_cids | ❌ No | Add |
| cycle complete | INFO | scanned, repairs | ✅ Yes | Add `duration_ms` |
| critical CID | WARN | cid, available, k | ✅ Yes | OK |
| normal CID | DEBUG | cid, available, target | ✅ Yes | OK |
| repair start | INFO | cid, severity | ✅ Yes | OK |
| repair success | INFO | cid, target | ✅ Yes | Add `duration_ms` |
| repair failure | WARN | cid, error | ✅ Yes | OK |

#### CraftCOM (`craftec-com/src/scheduler.rs`)

| Event | Level | Fields | Current | Needed |
|-------|-------|--------|---------|--------|
| program loaded | INFO | wasm_cid, bytes | ✅ Yes | OK |
| program started | INFO | wasm_cid | ✅ Yes | OK |
| program stopped | INFO | wasm_cid, reason | ✅ Yes | OK |
| program crashed | WARN | wasm_cid, crash_count, error | ✅ Yes | Add `backoff_ms` |
| program quarantined | ERROR | wasm_cid, crash_count | ✅ Yes | OK |
| fuel exhausted | DEBUG | wasm_cid | ❌ No | Add |

#### Event Bus (`craftec-node/src/event_bus.rs`)

| Event | Level | Fields | Current | Needed |
|-------|-------|--------|---------|--------|
| init | INFO | capacity | ✅ Yes | OK |
| published | DEBUG | event, receivers | ✅ Yes | OK |
| no receivers | WARN | event | ✅ Yes | OK |
| dispatch CidWritten | DEBUG | cid | ✅ Yes | Add `rlnc_triggered` |
| dispatch PeerConnected | INFO | node_id | ✅ Yes | OK |
| dispatch PeerDisconnected | INFO | node_id | ✅ Yes | OK |
| dispatch lagged | WARN | dropped_events | ✅ Yes | OK |

#### Node Lifecycle (`craftec-node/src/node.rs`)

| Event | Level | Fields | Current | Needed |
|-------|-------|--------|---------|--------|
| Step N: init | INFO | step_name | ✅ Yes | Add `duration_ms` per step |
| all subsystems ready | INFO | node_id, port | ✅ Yes | Add `total_init_ms` |
| bootstrap start | INFO | peers | ✅ Yes | OK |
| bootstrap complete | INFO | - | ✅ Yes | Add `connected_count` |
| shutdown initiated | INFO | - | ✅ Yes | OK |
| shutdown timeout | WARN | - | ✅ Yes | Add `tasks_remaining` |
| shutdown complete | INFO | - | ✅ Yes | Add `total_shutdown_ms` |

### Implementation Steps

1. Add `Instant` timing to all init steps in `node.rs` — report per-step and total duration
2. Add `Instant::now()` to all `put/get/execute/query` operations — report `duration_ms`
3. Add SWIM probe lifecycle logs (sent, ack, timeout, indirect)
4. Add RLNC encode/recode/decode lifecycle logs
5. Add periodic SWIM membership summary (every 10 protocol ticks)
6. Add scan cycle start/end timing
7. Add CraftSQL query start/complete logs with row counts

---

## Part E — Docker Multi-Node Test Infrastructure ✅ IMPLEMENTED

Built and tested in commits `ae90a79` (initial) and `d47da36` (fixes).

### Architecture

```
┌────────────────────────────────────────────────────────────┐
│              Docker Compose Network (craftec)               │
│                  subnet: 172.28.0.0/16                     │
│                                                            │
│  ┌──────────┐  ┌──────────┐  ┌──────────┐                │
│  │  node-1   │  │  node-2   │  │  node-3   │               │
│  │ (seed)    │  │           │  │           │               │
│  │ 172.28.1.1│  │ 172.28.1.2│  │ 172.28.1.3│               │
│  │ port:4433 │  │ port:4433 │  │ port:4433 │               │
│  └──────────┘  └──────────┘  └──────────┘                │
│                                                            │
│  ┌──────────┐  ┌──────────┐  ┌─────────────────────────┐ │
│  │  node-4   │  │  node-5   │  │   test-runner           │ │
│  │ 172.28.1.4│  │ 172.28.1.5│  │   172.28.1.100          │ │
│  │ port:4433 │  │ port:4433 │  │   docker:27-cli         │ │
│  └──────────┘  └──────────┘  │   + Docker socket mount  │ │
│                               └─────────────────────────┘ │
└────────────────────────────────────────────────────────────┘
```

### Key Design Decisions

- **Dockerfile**: `rust:latest` builder → `debian:bookworm-slim` runtime. Binary name is `craftec`. Non-root `craftec` user. `WORKDIR /data` for config file writes.
- **Environment overrides**: `CRAFTEC_DATA_DIR`, `CRAFTEC_LISTEN_PORT`, `CRAFTEC_BOOTSTRAP_PEERS` (implemented in `main.rs`).
- **Test runner**: Uses `docker:27-cli` with Docker socket mounted (`/var/run/docker.sock:ro`) so it can run `docker logs` to inspect node behavior. This is the key architectural choice — tests verify behavior via structured log patterns.
- **Nodes 2-5** bootstrap from node-1 (`172.28.1.1:4433`).

### Running Tests

```bash
# Build and run everything (test-runner runs automatically)
docker compose up --build

# Or start cluster separately
docker compose up -d --build node-1 node-2 node-3 node-4 node-5
docker compose run test-runner

# View specific node logs
docker compose logs -f node-1

# Tear down
docker compose down -v
```

### Test Scenarios (Not Yet Testable)

The following scenarios from the original plan require a lightweight HTTP/RPC test endpoint on the node so the test runner can trigger operations. Currently the node only speaks QUIC wire protocol, so the test runner can only observe via log patterns.

1. **Data Store & Retrieve** — PUT on node-1, read from node-3
2. **Health Scan & Repair** — kill node holding pieces, verify repair
3. **SQL Write & RPC Write** — owner writes, remote SignedWrite
4. **Concurrent Writes** — 100 concurrent INSERTs
5. **HLC Clock Skew** — simulate 600ms skew, verify rejection
6. **Degradation** — gracefully remove nodes, monitor repair

**Next step**: Add a small HTTP test endpoint to unlock these end-to-end lifecycle tests.

---

## Part F — Deep Subsystem Lifecycle Tests ✅ IMPLEMENTED

Implemented in commit `480311f`. Replaced the 10 surface-level ping tests with 37 deep log-based verification tests across 7 phases.

### Test Runner Architecture

The test runner uses `docker:27-cli` with Docker socket mounted, enabling it to run `docker logs` to inspect actual node behavior. Tests verify structured log patterns emitted by the verbose logging added in Part D.

### Test Phases

| Phase | Tests | What it verifies |
|-------|-------|-----------------|
| 1. Infrastructure | 3 | Container state, IPs, cluster size |
| 2. Node Init | 8 | Identity generation, init timing, all subsystem init logs (OBJ, RLNC, VFS, SQL, QUIC, event bus, SWIM, lock file) |
| 3. SWIM Convergence | 6 | SWIM loop running, peer join, peer discovery, probe-ack cycle, gossip piggyback |
| 4. Subsystem Bootstrap | 10 | Event bus wired, PendingFetches pruner, DHT pruner, health scanner, piece tracker, scheduler, storage bootstrap, adaptive piggyback, variable-K |
| 5. Stability | 4 | No panics, no corruption, no event lag, events processing |
| 6. Background Tasks | 4 | Health scan cycling, SWIM ticks, channel health, background activity |
| 7. Graceful Shutdown | 4 | SIGTERM delivery, shutdown sequence, departure detection, clean exit |

### Results

All 37 tests pass on a 5-node Docker cluster.

---

## Summary

### Fix Verification: 15/15 ✅ (original audit) + 7/7 ✅ (C1-C7) + 5/5 ✅ (code review bugs)

| ID | Description | Status |
|----|-------------|--------|
| T1 | HLC 64-bit with CAS + skew + replay | ✅ Verified |
| T2 | SWIM probe-ack nonce correlation | ✅ Verified (+ nonce mismatch bug fixed) |
| T3 | PendingFetches prune_stale + registered_at | ✅ Verified |
| T5 | Wire frame v1 17-byte header with HLC | ✅ Verified |
| T6 | SWIM Acquire/AcqRel ordering | ✅ Verified |
| T7 | SWIM entry().and_modify().or_insert() | ✅ Verified |
| T8 | Suspect timeout 5000ms | ✅ Verified |
| T9 | SQL conn mutex held through commit | ✅ Verified |
| T10 | request_id correlation | ✅ Verified |
| T11 | 30s stream read timeout (10s for repair) | ✅ Verified |
| T13 | DHT ProviderRecord TTL pruning | ✅ Verified |
| T15 | JoinSet + 5s timeout + abort_all | ✅ Verified |
| G1 | File-backed SQLite with data_dir | ✅ Verified |
| G2 | EventBus::sender() wired to OBJ+SQL | ✅ Verified |
| G3 | RLNC on CidWritten + recursion guard | ✅ Verified (+ race condition fixed) |
| C1 | RLNC encode off event loop via tokio::spawn | ✅ Fixed |
| C2 | SQL read/write connection separation + busy_timeout | ✅ Fixed |
| C3 | Adaptive SWIM piggyback max(4, ceil(log2(n))) | ✅ Fixed |
| C4 | Rate-limited batched storage bootstrap | ✅ Fixed |
| C5 | Periodic PendingFetches pruning (60s) | ✅ Fixed |
| C6 | Periodic DHT provider pruning (60s) | ✅ Fixed |
| C7 | Variable-K health scanning via PieceTracker | ✅ Fixed |

### Code Review Bug Fixes

| Bug | File | Description |
|-----|------|-------------|
| Probe nonce mismatch | swim.rs | `probe_with_ack` extracted nonce from message instead of allocating new one |
| Indirect probe target | swim.rs | Sent to delegates instead of target |
| Piece CID race | node.rs | Pre-compute CID and mark_piece_cid BEFORE store.put fires CidWritten |
| Missing busy_timeout | database.rs | Added PRAGMA busy_timeout = 5000 to write_conn |
| Silent serialization failure | node.rs | Replaced unwrap_or_default with proper error handling |

### Test Coverage

- **Unit tests**: 370 pass (363 original + 7 new), 0 failures
- **Docker tests**: 37 pass across 7 phases, 0 failures
- **Clippy**: Clean (`-D warnings`)
- **Format**: Clean (`cargo fmt --all -- --check`)

### Remaining Work

The 6 end-to-end lifecycle flows (upload, distribute, repair, scale, degrade, eviction) require a lightweight HTTP/RPC test endpoint on the node. Currently the node only speaks QUIC wire protocol, limiting the test runner to log-pattern observation.
