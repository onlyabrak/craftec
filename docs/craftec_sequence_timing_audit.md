# Craftec Sequence, Order & Timing Audit

**Scope:** All 11 crates, ~100 .rs source files  
**Commit:** `dd33869`  
**Spec reference:** Technical Foundation v3.3  
**Date:** 2026-03-03  

---

## Executive Summary

Networking correctness depends on **message ordering**, **event sequencing**, **clock discipline**, and **race-free state transitions**. This audit examines every subsystem for violations, missing guarantees, and timing hazards.

**Verdict: 4 Critical, 5 High, 6 Medium findings.**

The most severe issue is the total absence of Hybrid Logical Clocks (spec §42), which removes the foundation for distributed event ordering and replay prevention. Combined with fire-and-forget uni-streams lacking correlation IDs, SWIM atomics with relaxed ordering, and unprotected DashMap read-modify-write patterns, the system currently has no reliable mechanism to determine "what happened before what" across nodes.

---

## 1. Message Ordering

### 1.1 QUIC Uni-Stream Fire-and-Forget (MEDIUM)

**File:** `craftec-net/src/endpoint.rs` lines 214-244  
**Design:** Every `send_message()` opens a new unidirectional QUIC stream, writes postcard bytes, calls `finish()`.

**What works:**
- Each individual message is atomically delivered (stream-level reliability from QUIC).
- No partial message delivery — the receiver calls `read_to_end()` on each stream.
- QUIC guarantees stream-level ordering within a connection, but since we open a **new uni-stream per message**, inter-message ordering is only guaranteed if they're on the **same connection** and the receiver processes them sequentially.

**What's missing:**
- **No request-response correlation.** `PieceRequest` is sent on stream A; the `PieceResponse` arrives on a completely separate stream B. There is no request ID, nonce, or correlation token linking them. The `PendingFetches` system uses CID as a loose correlator, but if two different subsystems request the same CID simultaneously, responses cannot be attributed.
- **No message sequence numbers.** If two SWIM gossip messages arrive out of order (incarnation 5 before incarnation 3), the incarnation comparison handles it correctly, but for non-SWIM messages (e.g., two `HealthReport` messages for the same CID), the receiver has no way to know which is newer.

**Impact:** At small scale, QUIC connection multiplexing makes out-of-order delivery rare. At million-node scale with many concurrent connections, this becomes a real data freshness problem for HealthReport and ProviderAnnounce.

**Fix:** Add a `request_id: u64` field to `PieceRequest` / `PieceResponse` for correlation. Add an HLC timestamp (see §1.6) to all wire messages for ordering.

### 1.2 No Wire Frame Envelope (HIGH — already noted in G5)

**File:** `craftec-types/src/wire.rs` lines 155-177  
**Spec §23** requires a 9-byte header: `[type_tag:u32 | version:u8 | payload_len:u32]`.  
**Code:** Raw postcard bytes are written directly to the stream with no envelope.

**Impact on sequencing:** Without `payload_len`, the receiver must use `read_to_end()` which waits for stream FIN. This precludes multiplexing multiple messages on a single stream. If the design ever moves to bidirectional streams or stream reuse, the lack of framing would cause message boundary corruption.

**Impact on version ordering:** Without `version:u8`, rolling upgrades cannot negotiate protocol versions. A v2 message sent to a v1 node will deserialize as garbage.

### 1.3 Reply Path Race (MEDIUM)

**File:** `craftec-net/src/endpoint.rs` lines 441-460  
**Pattern:** When handler returns `Some(reply)`, the endpoint opens a **new uni-stream on the same connection** to send the reply.

```rust
if let Some(reply) = handler.handle_message(remote, msg).await {
    // Opens a new uni-stream for the reply
    match conn.open_uni().await { ... }
}
```

**Problem:** This reply arrives as an unsolicited inbound message on the remote side. The remote node's `handle_craftec_conn` loop will read it as a **new message** and dispatch it through the handler. A `Pong` reply works because the handler explicitly handles `Pong` as a no-op. But a `PieceResponse` reply will be dispatched to `PendingFetches::resolve()` — this **only works** because PendingFetches uses CID as a key, not a request-specific correlator.

**Race:** If node A sends `PieceRequest{CID=X}` to both node B and node C, and both respond, both responses feed into `PendingFetches::resolve()`. The first response resolves all waiters and drops subsequent ones. This is acceptable for piece delivery (any valid piece works), but the discarded response represents wasted bandwidth.

### 1.4 PendingFetches Has No Timeout (CRITICAL)

**File:** `craftec-net/src/pending.rs` lines 30-35  
```rust
pub fn register(&self, cid: &Cid) -> oneshot::Receiver<CodedPiece> {
    let (tx, rx) = oneshot::channel();
    self.waiters.entry(*cid).or_default().push(tx);
    rx
}
```

**Problem:** If a piece request is sent but the remote node never responds (network partition, crash, or just slow), the `oneshot::Receiver` will hang forever. The `oneshot::Sender` is stored in the DashMap indefinitely, creating a **memory leak** proportional to the number of unanswered requests.

**At million-node scale:** If 1% of piece requests fail silently (common in P2P), with 10,000 CIDs each requesting 32 pieces, that's 3,200 leaked waiters per scan cycle. Over hours, this exhausts memory.

**Fix:** The caller of `register()` must wrap the `rx.await` with `tokio::time::timeout()`. Also, `PendingFetches` needs a `prune_stale()` method (similar to `PieceTracker`) that periodically drops senders whose receivers have been dropped (indicating the caller timed out).

### 1.5 SWIM Probe-Ack Correlation Is Absent (HIGH)

**File:** `craftec-net/src/swim.rs` lines 360-408  
**Protocol:** `protocol_tick()` sends a `SwimPing` to a target. The SWIM protocol requires that the probe sender **wait for an ack** within a timeout, then escalate to `ping-req` through K random peers if no ack arrives, and finally suspect the target.

**Code:** The ping is sent via `send_message()` (fire-and-forget). There is **no await** for the ack. There is **no ping-req escalation**. There is **no probe timeout**.

```rust
for (target, msg) in probes {
    let ep = endpoint.clone();
    tokio::spawn(async move {
        if let Err(e) = ep.send_message(&target, &msg).await {
            // Only logs the send failure — does not trigger suspect
        }
    });
}
```

**What actually happens:** The remote node receives `SwimPing`, processes piggybacked gossip, and sends back a `SwimAlive` on a new uni-stream. But the `run_swim_loop` never reads this ack. The only way a node gets suspected is if another node explicitly sends `SwimSuspect` — but nobody does that today because the probe-ack-suspect cycle is incomplete.

**Impact:** Failure detection is non-functional. Nodes that crash are never detected. Dead nodes persist in the `Alive` state until the process restarts.

**Fix:** The SWIM tick must:
1. Send `SwimPing` to target.
2. Register a `PendingProbe` with a oneshot (similar to PendingFetches).
3. `tokio::time::timeout(ack_timeout)` on the probe response.
4. On timeout → send `PingReq` to K random peers (indirect probe).
5. On second timeout → `mark_suspect(target)`.

---

## 2. Event Ordering

### 2.1 Event Bus Has No Publishers (CRITICAL — already noted in G2)

**File:** `craftec-node/src/event_bus.rs`  
**File:** `craftec-node/src/node.rs` lines 457-533  

The event bus is created and a subscriber loop is spawned, but **no subsystem ever calls `event_bus.publish()`** during normal operation. The only publish call is during shutdown (line 550).

This means:
- `CidWritten` events are never emitted → DHT announcements after writes never happen through the event bus (the storage bootstrap does it once at startup, but new writes during runtime are not announced).
- `PeerConnected` / `PeerDisconnected` events are never emitted → the event dispatch loop's cleanup of `piece_tracker.remove_node()` and `dht.remove_node()` on disconnect never fires.
- `RepairNeeded` events are never emitted through the bus (repairs go through the mpsc channel directly from HealthScanner to RepairExecutor, which is correct, but the event bus subscriber handles this as a no-op anyway).

**Impact on ordering:** Without events, subsystems are decoupled in the wrong way — they don't know about state changes in other subsystems. When events are eventually wired in, the ordering guarantee must be documented: `tokio::sync::broadcast` guarantees FIFO order from a single sender, but if multiple tasks call `publish()` concurrently, the order between their events is non-deterministic.

### 2.2 Startup Sequencing Is Correct but Fragile (LOW)

**File:** `craftec-node/src/node.rs` lines 140-306  

The `CraftecNode::new()` initialization follows a strict dependency order (Steps 1-12) matching spec §57. Dependencies flow downward:

```
data_dir → KeyStore → CraftOBJ → RLNC → CID-VFS → CraftSQL
    → CraftCOM → EventBus → Endpoint → SWIM → HealthScanner → ProgramScheduler
```

**What works:** Each subsystem takes `Arc<>` references to its dependencies, and creation is sequential. No subsystem starts processing before the node calls `run()`.

**Risk:** The `run()` method spawns all background tasks **without waiting for them to initialize**. The storage bootstrap task (CID announcement) starts immediately and may complete before the SWIM loop has registered any peers. At startup with bootstrap peers, the order is:
1. Bootstrap connects to peers (line 332-340)
2. Storage bootstrap spawns and announces CIDs (line 343-370)
3. Accept loop spawns (line 373-396)
4. SWIM loop spawns (line 399-409)

Since bootstrap (step 1) runs before the accept loop (step 3), a race exists where a bootstrap peer's response may arrive before the accept loop is ready. In practice, iroh queues incoming connections, so this is low risk.

### 2.3 Shutdown Ordering (MEDIUM)

**File:** `craftec-node/src/node.rs` lines 546-573  

```rust
self.event_bus.publish(Event::ShutdownSignal);  // Step 1
let _ = self.shutdown_tx.send(());               // Step 2
tokio::time::sleep(Duration::from_millis(500)).await;  // Step 3
// Remove node.lock                               // Step 4
```

**Problem:** The 500ms sleep is a heuristic. There is no join/await on the spawned tasks. If the SWIM loop is mid-tick when shutdown fires, it may attempt to send a probe to a peer after the endpoint has been dropped. The `tokio::select!` in each loop checks `shutdown_rx.recv()` but there's a race between the sleep expiring and the task actually stopping.

**Fix:** Use `JoinHandle` tracking: collect all spawned task handles and `join_all()` with a timeout instead of a fixed 500ms sleep.

---

## 3. Timing & Clocks

### 3.1 No Hybrid Logical Clock — HLC (CRITICAL)

**Spec §42** defines a 64-bit HLC: `[48-bit ms wall clock | 16-bit logical counter]`
- Max skew tolerance: 500ms
- Persist every 100ms
- Required for: distributed event ordering, replay prevention (±30s window), wire message timestamps

**Code:** There is **zero HLC implementation** anywhere in the codebase. No struct, no clock, no timestamp field on any wire message.

**Impact:**
- **Replay attacks:** Without HLC timestamps, an attacker can record and replay any wire message indefinitely. There is no ±30s replay window check.
- **Event ordering across nodes:** Without a distributed clock, there is no way to determine causal ordering of events from different nodes. Two `ProviderAnnounce` messages for the same CID from different nodes cannot be ordered.
- **Conflict resolution:** The `SignedWrite` CAS mechanism uses `expected_root` CID for single-writer ordering, but there's no way to order events across different databases.

**Fix:** Implement `HybridClock` struct in `craftec-types`:
```rust
pub struct HybridClock {
    wall: AtomicU64,  // 48-bit ms
    logical: AtomicU16,
}
```
Add `hlc_timestamp: u64` to all `WireMessage` variants. On send: `hlc.now()`. On receive: `hlc.observe(msg.hlc_timestamp)` with 500ms max-skew rejection.

### 3.2 SWIM Timing Constants Mismatch Spec (HIGH — already noted)

| Parameter | Spec §18 | Code | 
|-----------|----------|------|
| Protocol period | 500ms | 500ms | ✅ |
| Suspect timeout | 5000ms | 1500ms | ❌ |

**File:** `craftec-net/src/swim.rs` line 45  
`const DEFAULT_SUSPECT_TIMEOUT: Duration = Duration::from_millis(1500);`

**Impact:** 1.5s is aggressive for a P2P network where latency between nodes can exceed 1s. Nodes behind slow connections will be falsely suspected and declared dead. At million-node scale, false suspicions cascade through gossip, creating membership churn storms.

### 3.3 HealthScanner Cursor Uses Relaxed Ordering (MEDIUM)

**File:** `craftec-health/src/scanner.rs` lines 91, 123, 137-138  

```rust
last_scan_index: AtomicUsize,
// ...
let start = self.last_scan_index.load(Ordering::Relaxed) % total;
// ...
self.last_scan_index.store((start + batch_size) % total, Ordering::Relaxed);
```

**Problem:** If `scan_cycle()` were ever called from multiple tasks (currently it's single-threaded in the `run()` loop), the `load + compute + store` sequence is a classic TOCTOU race. Two concurrent cycles could read the same `start`, scan the same batch, and advance the cursor identically — causing some CIDs to be scanned twice and others skipped.

**Current risk:** Low, because `scan_cycle()` is only called from the sequential `run()` loop. But the API is public (`pub async fn scan_cycle`), so a future caller could introduce the race.

**Fix:** Use `fetch_update` or wrap the cursor advance in the batch-size computation as a single atomic operation. Or document that `scan_cycle()` must only be called from a single task.

### 3.4 No Timeouts on QUIC Stream Reads (MEDIUM)

**File:** `craftec-net/src/endpoint.rs` lines 430, 494  

```rust
match stream.read_to_end(4 * 1024 * 1024).await {  // No timeout!
```

A malicious or buggy peer can open a uni-stream, send a partial message, and never close it. The `read_to_end()` call blocks that task indefinitely (up to the 4 MiB limit, but the issue is time, not size). Since each connection spawns a task that loops on `accept_uni()`, a single stalled stream blocks that task's processing of subsequent messages on the same connection.

**Fix:** Wrap with `tokio::time::timeout(Duration::from_secs(30), stream.read_to_end(4 * 1024 * 1024))`.

### 3.5 DHT Has No TTL or Re-Announcement Timer (MEDIUM)

**File:** `craftec-net/src/dht.rs`  
**Spec §18** requires DHT re-announce every 22 hours.

**Code:** Provider records are stored in a plain `DashMap<Cid, HashSet<NodeId>>` with no timestamps. Records are never expired. The only removal is explicit: `remove_provider()` or `remove_node()` (called on SWIM dead — but SWIM dead detection is broken, see §1.5).

**Impact:** Over time, the DHT accumulates stale provider records for nodes that left the network gracefully (without being SWIM-declared dead). A lookup for a CID may return providers that haven't been reachable for days.

**Fix:** Add `announced_at: Instant` to provider records. Implement `prune_stale()` with a 24h TTL. Add a periodic re-announcement loop that runs every 22h.

---

## 4. State Sequencing

### 4.1 SWIM Incarnation Uses Relaxed Ordering (HIGH)

**File:** `craftec-net/src/swim.rs` lines 91, 151, 190, 275, 335  

All `AtomicU64` operations on `incarnation` use `Ordering::Relaxed`:

```rust
self.incarnation.load(Ordering::Relaxed)
self.incarnation.fetch_add(1, Ordering::Relaxed)
```

**Problem:** `Ordering::Relaxed` provides no synchronization guarantees between threads. If two threads concurrently call `mark_suspect` (which triggers `fetch_add` on self-suspicion refutation), the resulting incarnation values are individually correct (atomic increment), but **other threads may observe the increments in different orders**.

Specifically, in `protocol_tick()`:
1. Thread A reads `incarnation = 5` (Relaxed).
2. Thread B increments to 6 (refuting a suspicion).
3. Thread A builds a `SwimPing` piggybacked with `SwimAlive { incarnation: 5 }`.
4. This stale incarnation 5 is broadcast, potentially allowing other nodes to re-suspect.

**Fix:** Use `Ordering::SeqCst` or at minimum `Ordering::Acquire` for loads and `Ordering::Release` for stores. The cost on x86 is zero (x86 provides TSO); on ARM it's a single barrier instruction.

### 4.2 DashMap Read-Modify-Write Patterns Are Non-Atomic (HIGH)

**File:** `craftec-net/src/swim.rs` lines 164-180 (mark_alive)  

```rust
let should_update = self.members.get(node_id)  // Step 1: READ
    .map(|e| match e.value() { /* compare incarnation */ })
    .unwrap_or(true);

if should_update {
    self.members.insert(*node_id, MemberState::Alive { incarnation });  // Step 2: WRITE
}
```

**Problem:** Between step 1 (read) and step 2 (write), another thread can modify the entry. Example:
1. Thread A: reads incarnation=3, decides should_update=true (incoming incarnation=5).
2. Thread B: marks node as Dead with incarnation=7.
3. Thread A: overwrites Dead{7} with Alive{5} — **violating monotonicity**.

This pattern exists in `mark_alive`, `mark_suspect`, and `mark_dead`.

**Fix:** Use `DashMap::entry()` with atomic insert-or-update:
```rust
self.members.entry(*node_id).and_modify(|state| {
    // Atomic read-compare-modify within the entry lock
    match state { ... }
}).or_insert(MemberState::Alive { incarnation });
```

### 4.3 CraftSQL Root CID Update Is Not Atomic with Execute (MEDIUM)

**File:** `craftec-sql/src/database.rs` lines 209-255  

```rust
pub async fn execute(&self, sql: &str, writer: &NodeId) -> Result<()> {
    // ...
    let conn = self.conn.lock().await;  // Mutex protects SQL execution
    conn.execute(sql, ()).await?;
    drop(conn);                          // ← Mutex released HERE

    // VFS write and commit happen OUTSIDE the mutex
    self.vfs.write_page(page_num, &page)?;
    let new_root = self.vfs.commit().await?;
    *self.root_cid.write() = new_root;    // RwLock write
}
```

**Problem:** The `tokio::sync::Mutex` on `conn` is dropped **before** the VFS commit. If two `execute()` calls run concurrently (different tasks, same owner), the sequence could be:
1. Task A: executes SQL, drops conn mutex.
2. Task B: executes SQL, drops conn mutex.
3. Task A: commits VFS, updates root_cid to Root_A.
4. Task B: commits VFS, updates root_cid to Root_B.

Both VFS commits succeed, but Root_B doesn't include Task A's page writes because VFS dirty pages are shared (via `Mutex<HashMap>`). The `dirty_pages` mutex in VFS (`parking_lot::Mutex`) is held only during `write_page` and released before `commit`. So Task B's `write_page` could interleave with Task A's VFS operations.

**Current mitigation:** The single-writer model means only the owner can execute. But the owner could submit concurrent RPC writes from different network paths (e.g., two browser tabs). The `RpcWriteHandler` does a CAS check, but the CAS is checked **before** execution — so two writes with the same expected_root could both pass CAS and then interleave.

**Fix:** Hold the conn mutex through the entire execute-commit cycle:
```rust
let conn = self.conn.lock().await;
conn.execute(sql, ()).await?;
self.vfs.write_page(page_num, &page)?;
let new_root = self.vfs.commit().await?;
*self.root_cid.write() = new_root;
drop(conn);  // Release AFTER commit
```

### 4.4 VFS Dirty Pages Not Protected During Commit (MEDIUM)

**File:** `craftec-vfs/src/vfs.rs` lines 207-249  

```rust
pub async fn commit(&self) -> Result<Cid> {
    let dirty: HashMap<u32, Vec<u8>> = {
        let mut guard = self.dirty_pages.lock();
        std::mem::take(&mut *guard)  // Drain all dirty pages atomically
    };
    // ... store each page ... compute root ...
}
```

**What works:** The `std::mem::take` atomically drains all dirty pages under the lock. Any `write_page()` call that occurs during `commit()` will write into a fresh empty HashMap and be included in the **next** commit.

**What's risky:** If `write_page()` is called between the dirty drain and the root CID update (line 240: `*self.current_root.write() = Some(new_root)`), the new page will be orphaned — it sits in the dirty map waiting for a commit, but the root CID has already advanced to a state that doesn't include it. The page will be picked up on the next commit, which is correct, but during the interval the root CID doesn't reflect the latest write.

This is actually fine for the single-writer model (writes are serialized by the SQL mutex), but it's a subtle ordering dependency that should be documented.

---

## 5. Findings Summary

| ID | Severity | Area | Finding |
|----|----------|------|---------|
| T1 | **CRITICAL** | Clocks | No HLC implementation (spec §42) — no distributed ordering, no replay prevention |
| T2 | **CRITICAL** | SWIM | Probe-ack cycle not implemented — failure detection non-functional |
| T3 | **CRITICAL** | Pending | PendingFetches has no timeout — memory leak on unanswered requests |
| T4 | **CRITICAL** | Events | Event bus has no publishers (same as G2) — subsystems decoupled incorrectly |
| T5 | **HIGH** | Wire | No frame envelope (same as G5) — no versioning, no length framing |
| T6 | **HIGH** | SWIM | Incarnation uses Relaxed atomics — stale values may be broadcast |
| T7 | **HIGH** | SWIM | DashMap read-modify-write is non-atomic — incarnation monotonicity can break |
| T8 | **HIGH** | SWIM | Suspect timeout 1.5s vs spec 5s — false positives at scale |
| T9 | **HIGH** | SQL | Execute drops conn mutex before VFS commit — concurrent writes can interleave |
| T10 | **MEDIUM** | Network | No request-response correlation IDs — responses unattributable at scale |
| T11 | **MEDIUM** | Network | No timeout on QUIC stream reads — stalled peers block task |
| T12 | **MEDIUM** | Network | Reply path sends unsolicited message — works by CID convention, not by protocol |
| T13 | **MEDIUM** | DHT | No TTL or re-announcement — stale providers accumulate indefinitely |
| T14 | **MEDIUM** | Health | Scanner cursor Relaxed atomic — TOCTOU if called concurrently (low current risk) |
| T15 | **MEDIUM** | Shutdown | Fixed 500ms sleep instead of task join — races on shutdown |

---

## 6. Priority Remediation Order

### Phase 1 — Must fix before any multi-node testing
1. **T2: SWIM probe-ack cycle** — Without this, failure detection is broken. No node will ever be suspected or declared dead.
2. **T3: PendingFetches timeout** — Wrap `rx.await` with `tokio::time::timeout(30s)`. Add `prune_stale()`.
3. **T4: Wire event bus publishers** — At minimum, emit `PeerConnected`/`PeerDisconnected` from the accept loop and `CidWritten` from the write path.

### Phase 2 — Must fix before network testing at scale
4. **T1: HLC implementation** — Add the 64-bit HLC struct, timestamp all wire messages, implement ±30s replay window.
5. **T7: DashMap entry-based updates** — Convert SWIM's read-then-write to atomic entry operations.
6. **T6: SeqCst atomics for incarnation** — Change all incarnation loads/stores to SeqCst.
7. **T9: Hold SQL mutex through commit** — Extend the critical section to cover VFS commit.

### Phase 3 — Required for production
8. **T5: Wire frame envelope** — Add the 9-byte header per spec §23.
9. **T8: Fix suspect timeout to 5s** — Match spec §18.
10. **T10: Correlation IDs** — Add `request_id: u64` to request/response pairs.
11. **T11: Stream read timeouts** — Add 30s timeout to all `read_to_end` calls.
12. **T13: DHT TTL** — Add `announced_at` timestamp, 24h TTL, 22h re-announce loop.
13. **T15: Task join on shutdown** — Collect JoinHandles, `join_all()` with timeout.

---

## 7. What's Done Right

Credit where due — several sequencing aspects are well-designed:

1. **Startup order matches spec §57** — Steps 1-12 are correctly sequenced with clear dependency flow.
2. **CAS on root CID** — The compare-and-swap pattern in `check_cas()` correctly prevents stale writes when used with the single-writer model.
3. **SWIM incarnation monotonicity logic** — The `mark_alive`, `mark_suspect`, `mark_dead` comparison logic correctly implements the SWIM state machine (higher incarnation wins). The implementation just needs atomic protection.
4. **VFS dirty page drain** — `std::mem::take` under a mutex is a clean atomic drain pattern.
5. **Broadcast channel for events** — `tokio::sync::broadcast` provides correct FIFO ordering within a single sender.
6. **Snapshot isolation** — VFS `snapshot()` correctly pins the root CID and page index, immune to concurrent commits.
7. **Natural Selection coordinator ranking** — Deterministic total order (uptime → reputation → NodeID) with no randomness means all nodes independently elect the same coordinator.
