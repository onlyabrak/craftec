# Craftec Full Audit Report — Code, Process, Data & Features

**Commit**: `dd33869`
**Spec Reference**: Technical Foundation v3.3 (93 pages, 60 sections)
**Scope**: 11 crates, ~100 .rs files, ~12,000 lines of Rust
**Date**: 2026-03-03

---

## Executive Summary

The codebase is structurally sound. All 11 crates compile, 204+ tests pass, the dependency graph follows spec initialization order, and zero critical design constraints are violated (no CRDT, no per-CID gossip topics, single-writer CraftSQL, RLNC client-side-only decode, BLAKE3 raw 32-byte CIDs, filesystem-as-index for CraftOBJ, kernel-level scheduler). The node can boot, join a SWIM cluster, accept connections, serve piece requests, handle signed RPC writes, run health scans, execute WASM agents, and gracefully shut down.

However, 5 gaps block end-to-end correctness, and 17 spec sections have no implementation at all. The system is approximately **40% feature-complete** against the full v3.3 spec (the "70%" in the prior subagent audit measured only implemented-crate coverage, not full spec coverage).

---

## Part 1: Code Audit — Per-Crate Findings

### 1.1 craftec-types — COMPLETE

All types fully implemented: `Cid` (32-byte BLAKE3), `NodeId`/`NodeKeypair` (Ed25519 via ed25519-dalek), `WireMessage` (12 variants), `CodedPiece`, `Event` (7 variants), `NodeConfig` (serde JSON). postcard serialization for wire encoding/decoding. No issues.

### 1.2 craftec-crypto — COMPLETE

- `KeyStore`: persistent Ed25519 key on disk, `node.key` file, auto-generate on first run.
- `sign`/`verify`: Ed25519 via ed25519-dalek.
- `hommac`: HomMAC with GF(2^8) multiplication (AES polynomial 0x11B), `compute_tag`, `verify_tag`, `combine_tags` (homomorphic linear combination). Tests verify homomorphic property.
- No issues.

### 1.3 craftec-rlnc — COMPLETE

- `RlncEncoder`: splits data into k pieces, generates random GF(2^8) coding vectors, computes HomMAC tags. `target_pieces = ceil(redundancy(k) * k)` where `redundancy(k) = 2.0 + 16/k`. Correct.
- `RlncDecoder`: Gaussian elimination with full pivot selection. `is_decodable()` checks rank == k. Client-side only per spec.
- `RlncRecoder`: combines 2+ coded pieces with fresh random coefficients. Uses `combine_tags` for HomMAC tag recombination. Correct.
- `RlncEngine`: Arc-shareable wrapper for encoder/decoder/recoder.
- No issues.

### 1.4 craftec-obj — COMPLETE (with 1 gap)

- `ContentAddressedStore`: BLAKE3 hash → hex filename under `obj/{shard}/`, LRU cache (lru crate), bloom filter (bloomfilter crate), `put`/`get`/`list_cids`.
- Shard = first 2 hex chars of CID = 256 subdirs. Correct per spec §27.
- **Gap G2**: `put()` does not publish `CidWritten` event to event bus. The store has no reference to EventBus. This means the async write path (Step 8 in spec §53: "Enqueue RLNC encoding via Event Bus") never fires.

### 1.5 craftec-vfs — FUNCTIONAL (with 1 critical gap)

- `CidVfs`: page-level abstraction over CraftOBJ. Configurable page size (default 4096, spec says 16384).
- `read_page`/`write_page`/`commit`: dirty page tracking, BLAKE3 hash on commit, CID mapping via `page_map` (in-memory HashMap), root CID computed as BLAKE3 of sorted page index.
- **Gap G1**: This is NOT a real SQLite VFS. There is no `xRead`/`xWrite`/`xSync` implementation, no SQLite VFS registration (no `sqlite3_vfs` struct). CraftSQL uses libsql's in-memory database, not CidVfs. The VFS exists as a standalone layer but is not wired into SQLite's I/O path. This is the single most critical gap — it means SQLite pages are NOT content-addressed.

### 1.6 craftec-sql — FUNCTIONAL (with VFS gap inherited)

- `CraftDatabase`: wraps libsql `Database` (in-memory), tracks `owner` (NodeId) and `root_cid` (AtomicCell). Provides `execute(sql, &writer)` with owner check, `query(sql)`, and root CID tracking.
- `RpcWriteHandler`: validates signature, checks CAS (Compare-And-Swap) on expected_root, rejects non-owner writes. `SignedWrite` struct with `build_signed_payload` helper. Correct design.
- `ColumnValue` enum for query results: Integer, Real, Text, Blob, Null. Good.
- **Inherited Gap**: Since CidVfs is not registered as SQLite VFS, the database runs in-memory only. Root CID updates are synthetic (BLAKE3 of SQL statement, not of actual page content). The CAS mechanism works but against synthetic roots, not real content-addressed page trees.

### 1.7 craftec-net — FUNCTIONAL (core networking operational)

- `CraftecEndpoint`: wraps `iroh::Endpoint`, ALPN `/craftec/1`, QUIC transport, `connect_to_peer`, `bootstrap`, `accept_loop` dispatching to `ConnectionHandler` trait.
- `SwimMembership`: HashMap-based member table with states (Alive/Suspect/Dead), incarnation numbers, `mark_alive`/`mark_suspect`/`mark_dead`/`protocol_tick` (promotes suspect→dead after timeout). `handle_message` processes SwimJoin/SwimAlive/SwimSuspect/SwimDead/SwimPing. `run_swim_loop` spawns periodic tick.
- `DhtProviders`: in-memory DashMap<Cid, HashSet<NodeId>>. `announce_provider`, `get_providers`, `remove_node`. `announce_cid_to_peers` broadcasts to all connected SWIM members.
- `PendingFetches`: oneshot channel map for piece request/response rendezvous.
- `ConnectionPool`: DashMap<NodeId, Arc<Connection>> with `get`/`insert`/`remove`/`connected_peers`.
- **Observations**:
  - SWIM tick interval is 1 second (spec says 500ms — **minor deviation**)
  - SWIM suspicion timeout is 1.5 seconds (spec says 5 seconds — **acceptable, more aggressive**)
  - No SWIM indirect probes (SWIM_PING_REQ) — spec implies them but doesn't strictly require them
  - DHT is local-only (no iroh Kademlia integration yet) — acceptable for current phase
  - No HELLO message exchange on connection (spec §21/§22: capability bits, protocol negotiation)
  - No 9-byte wire frame envelope (spec §23: `type_tag:u32 | version:u8 | payload_len:u32`). Current implementation uses raw postcard serialization.

### 1.8 craftec-health — FUNCTIONAL

- `HealthScanner`: periodic scan of PieceTracker, configurable scan_percent (default 1% = spec-correct), emits `RepairRequest` via mpsc channel. `scan_cycle` iterates tracked CIDs, checks available count vs target (`ceil(redundancy(k) * k)` for k=32). Severity classification: critical (<k), warning (<target), healthy.
- `PieceTracker`: DashMap<Cid, Vec<PieceHolder>> tracking per-CID piece availability. `record_piece`, `remove_node` (cascades across all CIDs), `available_count`.
- `RepairExecutor`: receives RepairRequests, fetches pieces from peers via endpoint, recodes via RLNC engine, distributes new piece. Well-structured.
- **Observations**:
  - Natural Selection Coordinator (spec §30: rank by uptime/reputation/NodeID) is NOT implemented — any node holding pieces acts as coordinator. This means duplicate repair work at scale.
  - No "1 piece per CID per cycle" rate limiting (spec §30).
  - HealthScanner sorts by CID but not by last-health-check timestamp (spec says oldest-first).

### 1.9 craftec-com — FUNCTIONAL (with 1 high gap)

- `ComRuntime`: Wasmtime engine with fuel metering, `execute_agent` compiles WASM, creates Store with HostState, calls exported function. Correct.
- `HostState`: holds Arc references to ContentAddressedStore, CraftDatabase, KeyStore. Host functions: `craft_store_get`, `craft_store_put`, `craft_sql_query`, `craft_sign`, `craft_read_result`. All implemented.
- `ProgramScheduler`: DashMap<String, ProgramEntry> tracking program state (Pending/Running/Stopped/Quarantined/Failed). `start_program`/`stop_program`/`list_programs`/`get_status`.
- **Gap G4**: `start_program()` only changes state to `Running` in the DashMap. **No `tokio::spawn` is called**. The WASM binary is never loaded from CraftOBJ, never compiled, never executed. No keepalive, no restart-on-failure, no crash detection. The scheduler is a state machine only.

### 1.10 craftec-node — FUNCTIONAL (orchestrator)

- `CraftecNode::new()`: 12-step initialization matching spec §57 Join Path. All subsystems created in correct dependency order. Clean, well-documented.
- `CraftecNode::run()`: bootstraps peers, spawns accept_loop, SWIM loop, health scan + repair executor, event dispatch loop. Blocks on Ctrl+C/SIGTERM. Graceful shutdown: publishes ShutdownSignal, broadcasts to tasks, removes node.lock.
- `NodeMessageHandler`: dispatches all WireMessage variants — Ping→Pong, PieceRequest→PieceResponse (from local store), PieceResponse→PendingFetches, ProviderAnnounce→DhtProviders, HealthReport→PieceTracker, SignedWrite→RpcWriteHandler. SWIM messages handled separately by swim connection handler.
- `EventBus`: broadcast channel, subscribe/publish. Used in event dispatch loop.
- Storage bootstrap: on startup, lists all local CIDs and announces them to DHT peers.
- **Observation**: Event dispatch loop handles CidWritten→DHT announce, PeerDisconnected→cleanup, DiskWatermarkHit→warn, PageCommitted→log, ShutdownSignal→break. Good.

### 1.11 craftec-tests — COMPREHENSIVE

- **multi_node.rs** (10 tests): SWIM 5-node discovery, RLNC distribute-across-nodes-and-decode, RLNC recode-at-intermediate, wire message all-variants round-trip, Ed25519 cross-node verification, HomMAC integrity, content store/retrieve by CID, full pipeline encode→sign→transmit→verify→decode, RLNC large data (32KB k=32), multiple CIDs independent decode.
- **scale_scenarios.rs** (8 tests): 10-node SWIM convergence, 10-node concurrent write throughput, SWIM churn (join+leave), repair storm prevention, PieceTracker remove_node cascade at scale, SWIM suspect→dead timeout, 10 independent databases, RLNC 20 CIDs across 10 nodes.
- **subsystem_integration.rs** (13 tests): RPC signed write E2E, CAS conflict detection, non-owner write rejection, store put/get roundtrip, DHT provider announce/query, HealthScanner detects under-replication, PendingFetches roundtrip, SQL full write/query roundtrip, store list_cids, CraftCOM execute with HostState, RLNC store→retrieve→decode, VFS snapshot isolation, multiple sequential RPC writes.
- **Assessment**: Tests are thorough for the implemented surface area. However, they test subsystems in isolation — no tests exercise the full write path (SQL→VFS→OBJ→RLNC→distribute→DHT) end-to-end because the VFS bridge doesn't exist.

---

## Part 2: Process Flow Audit

### 2.1 Write Path (Spec §53: 11 steps)

| Step | Spec | Implemented? | Notes |
|------|------|:---:|-------|
| 1 | User SQL statement, CraftSQL begins transaction | YES | `CraftDatabase::execute()` |
| 2 | SQLite modifies pages, CID-VFS accumulates dirty pages | **NO** | VFS not registered with SQLite (G1) |
| 3 | xSync triggers commit | **NO** | No xSync (G1) |
| 4 | BLAKE3 hash dirty pages | PARTIAL | CidVfs.commit() does this, but not triggered by SQLite |
| 5 | Write pages to CraftOBJ | PARTIAL | CidVfs.commit() does this, but synthetic |
| 6 | Atomically update root CID | PARTIAL | AtomicCell swap, but synthetic root |
| 7 | Return SQLITE_OK | YES | libsql returns success |
| 8 | Enqueue RLNC encoding via Event Bus | **NO** | Event bus not published from store (G2, G3) |
| 9 | RLNC encode pages | **NO** | Never triggered (G3) |
| 10 | Distribute coded pieces to peers | **NO** | Never triggered |
| 11 | Announce root CID via DHT/Pkarr/SWIM | PARTIAL | DHT announce works if manually triggered |

**Write path verdict**: Steps 1 and 7 work (SQL executes in-memory). Steps 2-6 exist as code but are disconnected. Steps 8-11 are not triggered. The sync write path (Steps 1-7) is functional for SQL correctness but not for content-addressing. The async write path (Steps 8-11) is fully absent.

### 2.2 Read Path (Spec §54: 11 steps)

| Step | Spec | Implemented? | Notes |
|------|------|:---:|-------|
| 1 | SQL query, pin root CID | YES | `CraftDatabase::query()` |
| 2 | SQLite requests page via xRead | **NO** | No VFS (G1) |
| 3 | Check hot page LRU | **NO** | VFS has a page_map but not integrated |
| 4 | Resolve page CID from page index | **NO** | — |
| 5 | Check local CID content cache | **NO** | — |
| 6 | Check bloom filter | YES (standalone) | CraftOBJ has bloom filter |
| 7 | Singleflight dedup | **NO** | Not implemented |
| 8 | DHT get_providers | YES (in-memory) | DhtProviders exists |
| 9 | Select provider, fetch via QUIC | YES | PieceRequest/PieceResponse handler works |
| 10 | Verify BLAKE3 | YES (standalone) | CraftOBJ verifies on get |
| 11 | Cache and return | PARTIAL | LRU cache exists |

**Read path verdict**: SQL queries work against in-memory libsql. The CID-based read path (Steps 2-7) is not connected. Network fetch (Steps 8-9) works for explicit piece requests but not for VFS page reads.

### 2.3 Repair Path (Spec §55: 8 steps)

| Step | Spec | Implemented? | Notes |
|------|------|:---:|-------|
| 1 | Health scanner runs periodically | YES | HealthScanner with configurable interval |
| 2 | Query piece availability per CID | YES | PieceTracker.available_count() |
| 3 | Compute piece rank, detect under-replication | YES | Severity classification works |
| 4 | Fetch coded pieces from peers | YES | RepairExecutor uses endpoint |
| 5 | Verify HomMAC tags | PARTIAL | HomMAC verify exists but not called in repair path |
| 6 | Recode with fresh coefficients | YES | RlncRecoder.recode() |
| 7 | Distribute new piece | YES | RepairExecutor distributes |
| 8 | Update availability map | PARTIAL | PieceTracker updated |

**Repair path verdict**: Structurally complete. Missing HomMAC verification on fetched pieces (Step 5) and Natural Selection Coordinator for dedup.

### 2.4 Agent Execution Path (Spec §39: load → instantiate → execute → terminate)

| Step | Spec | Implemented? | Notes |
|------|------|:---:|-------|
| Load WASM binary from CraftOBJ by CID | **NO** | `start_program()` doesn't load (G4) |
| Compile via Wasmtime | YES | `ComRuntime::execute_agent()` compiles |
| Create fresh Store with HostState | YES | Correct |
| Call exported function | YES | Correct |
| Collect return value | YES | Correct |
| Terminate, reclaim memory | YES | Store dropped on return |
| Keepalive / restart-on-failure | **NO** | ProgramScheduler is state-only (G4) |

**Agent path verdict**: One-shot execution works (proven by tests). Long-running program lifecycle (load by CID, keepalive, restart, quarantine) is not implemented.

### 2.5 SWIM Membership (Spec §18)

| Feature | Implemented? | Notes |
|---------|:---:|-------|
| SwimJoin / SwimAlive / SwimSuspect / SwimDead | YES | All 4 state transitions |
| SwimPing with piggyback | YES | SwimPing carries piggyback messages |
| Suspect → Dead promotion on timeout | YES | protocol_tick() with configurable timeout |
| Incarnation number refutation | YES | Higher incarnation overrides suspect |
| Tick interval | YES (1s) | Spec says 500ms — minor |
| Indirect probes (SWIM_PING_REQ) | **NO** | Not implemented |
| Member list gossip | PARTIAL | Gossip via piggybacking, no full SWIM_MEMBER_LIST |

**SWIM verdict**: Core protocol works, proven by 10-node convergence tests. Indirect probes missing (reduces false-positive failure detection).

### 2.6 Join Path (Spec §57: 12 steps)

| Step | Implemented? | Notes |
|------|:---:|-------|
| 1. Generate keypair, NodeID | YES | KeyStore in Step 3 |
| 2. Write node.lock | YES | Step 2 in CraftecNode::new() |
| 3. Load config, validate | YES | load_or_create_config() |
| 4. Init subsystems in order | YES | 12 steps, correct order |
| 5. Bootstrap: relay, DNS seeds, IPs | PARTIAL | Connects to configured peers, no DNS seeds |
| 6. TLS 1.3, ALPN negotiation | PARTIAL | ALPN `/craftec/1`, no HELLO/capability |
| 7. SWIM JOIN | YES | run_swim_loop spawned |
| 8. Admission checks | **NO** | No subnet diversity, no ban list |
| 9. Pkarr DNS announce | **NO** | Not implemented |
| 10. Request peer lists | **NO** | Not implemented |
| 11. Storage bootstrap | YES | list_cids → announce to DHT on startup |
| 12. Full participation | PARTIAL | Accept, serve, health scan — yes. Agents — no. |

---

## Part 3: Data Flow Audit

### 3.1 CID Lifecycle

| Phase | Implemented? | Notes |
|-------|:---:|-------|
| **Creation**: BLAKE3(data) → 32-byte raw CID | YES | CraftOBJ, CidVfs, Cid::from_data() |
| **Storage**: CID = filename on filesystem | YES | Hex-encoded, sharded by first byte |
| **Replication**: RLNC encode → distribute pieces | **NO** | Encode never triggered from write path |
| **Discovery**: DHT provider records | PARTIAL | In-memory DhtProviders, no iroh DHT |
| **Health**: HealthScan → repair if under-replicated | YES | Functional |
| **Eviction**: Local eviction policy | **NO** | Spec §31 — not implemented |
| **GC**: Mark-and-sweep | **NO** | Spec §31 — not implemented |

### 3.2 Event Propagation

| Event | Publishers | Consumers | Status |
|-------|-----------|-----------|--------|
| CidWritten | **NONE** (should be CraftOBJ.put) | RLNC, GC, DHT announce | **BROKEN** — G2 |
| PageCommitted | **NONE** (should be CidVfs.commit) | Pkarr, Observability | **BROKEN** — G2 |
| PeerConnected | **NONE** (should be accept_loop) | SWIM, Reputation | **BROKEN** |
| PeerDisconnected | **NONE** (should be connection drop) | SWIM, Pool cleanup | **BROKEN** |
| RepairNeeded | HealthScanner (via mpsc, not event bus) | Repair executor | WORKS (via mpsc channel, not event bus) |
| DiskWatermarkHit | **NONE** | GC, Job Coordinator | **BROKEN** |
| ShutdownSignal | CraftecNode::run() on shutdown | All tasks | WORKS |

**Event bus verdict**: 5 of 7 event types have no publishers. The event dispatch loop in `node.rs` correctly handles all event types, but only ShutdownSignal and (partially) CidWritten (via manual announce in storage bootstrap) ever fire. The repair path bypasses the event bus entirely (uses direct mpsc channel), which is acceptable but diverges from spec §52.

### 3.3 Wire Protocol

| Message | Spec §23 Tag | Implemented? | Handler? |
|---------|-------------|:---:|:---:|
| HELLO | 0x01000001 | **NO** | — |
| PING | 0x01000002 | YES | Ping→Pong |
| PONG | 0x01000003 | YES | Logged |
| WANT_CID | 0x02000001 | **NO** | — |
| HAVE_CID | 0x02000002 | **NO** | — |
| DONT_HAVE | 0x02000003 | **NO** | — |
| PIECE_DATA | 0x02000004 | PARTIAL | PieceRequest/PieceResponse (different name) |
| SWIM_PING | 0x03000001 | YES | SwimPing |
| SWIM_ACK | 0x03000002 | **NO** | — |
| SWIM_MEMBER_LIST | 0x03000003 | **NO** | Individual SwimAlive/Suspect/Dead instead |
| ATTEST_BROADCAST | 0x04000001 | **NO** | — |
| SIGNED_WRITE | 0x06000001 | YES | Handler processes RPC writes |
| WRITE_RESULT | 0x06000002 | **NO** | No response to writer |
| DISCONNECT | 0x05000001 | **NO** | — |

**Wire protocol verdict**: 6 of 14 message types implemented. SWIM messages use a different structure (individual SwimJoin/SwimAlive/SwimSuspect/SwimDead vs spec's SWIM_PING/SWIM_ACK/SWIM_MEMBER_LIST) — functionally equivalent but structurally different. No 9-byte frame envelope (G5). No HELLO capability exchange.

### 3.4 State Management

| State | Storage | Persistence | Notes |
|-------|---------|:-----------:|-------|
| Ed25519 keypair | `node.key` file | YES | Correct |
| Node config | `craftec.json` | YES | Correct |
| CraftOBJ blobs | Filesystem `obj/` | YES | Correct |
| CraftSQL data | libsql in-memory | **NO** | Lost on restart |
| SWIM membership | In-memory HashMap | NO | Expected (ephemeral) |
| DHT providers | In-memory DashMap | NO | Expected (ephemeral) |
| Piece tracker | In-memory DashMap | NO | Expected (ephemeral) |
| Page map (VFS) | In-memory HashMap | NO | Should be persisted as root CID |
| Program scheduler | In-memory DashMap | NO | Should reload from whitelist |

**State verdict**: CraftOBJ persistence is correct. CraftSQL data is NOT persisted (in-memory libsql) — this is the G1 consequence. On node restart, all SQL data is lost.

---

## Part 4: Feature Completeness vs Spec

### Spec Coverage Matrix

| Section | Spec Title | Status | Coverage |
|---------|-----------|--------|----------|
| §11-13 | Configuration, First-Run Init | Implemented | 90% |
| §14-16 | BLAKE3, HomMAC, RLNC Coding | Implemented | 95% |
| §17 | Node Identity (Ed25519) | Implemented | 100% |
| §18 | Bootstrap & Discovery | Partial | 40% |
| §19 | Peer Reputation & Trust | **Not implemented** | 0% |
| §20 | Network Admission | **Not implemented** | 0% |
| §21 | Connection Lifecycle | Partial | 50% |
| §22 | Protocol Negotiation (ALPN/HELLO) | **Not implemented** | 5% |
| §23 | Wire Protocol | Partial | 40% |
| §24 | Backpressure & Flow Control | Partial | 30% |
| §25 | Connection Pool Management | Partial | 40% |
| §26 | NAT Traversal | Delegated to iroh | 60% |
| §27 | Content-Addressed Store | Implemented | 90% |
| §28 | RLNC Erasure Coding | Implemented | 95% |
| §29 | Piece Distribution | **Not implemented** | 0% |
| §30 | Health Scanning & Repair | Partial | 60% |
| §31 | Local Eviction Policy | **Not implemented** | 0% |
| §32 | Disk Space Management | **Not implemented** | 0% |
| §33 | CID-VFS Implementation | Partial | 30% |
| §34 | Commit Flow | Partial | 30% |
| §35 | WAL Elimination / MVCC | Partial | 40% |
| §36 | Page Cache | Partial | 20% |
| §37 | Root CID Publication | **Not implemented** | 0% |
| §38 | Distributed Compute Engine | Partial | 60% |
| §39 | Agent Lifecycle | Partial | 50% |
| §40 | Host Functions | Implemented | 80% |
| §41 | Attestation Flow | **Not implemented** | 0% |
| §42 | Clock & Time (HLC) | **Not implemented** | 0% |
| §43 | Observability & Metrics | **Not implemented** | 0% |
| §44 | Resource Management | **Not implemented** | 0% |
| §45 | Security Hardening | **Not implemented** | 0% |
| §46 | Testing Strategy | Partial | 40% |
| §47 | Error Classification | Partial | 30% |
| §48 | Task Scheduler & Program Lifecycle | Partial | 30% |
| §49 | Request Coalescing / Singleflight | **Not implemented** | 0% |
| §50 | Batch Writer | **Not implemented** | 0% |
| §51 | Background Job Coordinator | **Not implemented** | 0% |
| §52 | Event Bus | Implemented (struct) | 30% |

**Sections fully or mostly implemented**: 9 of 42 (~21%)
**Sections partially implemented**: 17 of 42 (~40%)
**Sections not implemented**: 16 of 42 (~38%)

---

## Part 5: Critical Gap Summary

### Priority 1 — Blocks end-to-end data path

| # | Gap | Impact | Fix Complexity |
|---|-----|--------|---------------|
| G1 | **CID-VFS not registered as SQLite VFS** — libsql runs in-memory, pages never flow through CidVfs, SQL data not content-addressed, not persisted | All data lost on restart; CraftSQL is a regular in-memory DB, not a distributed one | HIGH — requires implementing sqlite3_vfs C API or using libsql's VFS extension point |
| G2 | **Event bus has no publishers** — CraftOBJ.put() and CidVfs.commit() don't emit events | Async write path (RLNC encode, distribute, DHT announce) never triggers | LOW — pass EventBus Arc to store and VFS, call publish() |
| G3 | **RLNC encoding never triggered on write** — PieceRequest handler serves raw blobs with identity coding vector `[1]` | Nodes serve unprotected data, no erasure redundancy, no HomMAC protection on served pieces | MEDIUM — wire CidWritten event → RLNC encode pipeline |

### Priority 2 — Blocks program execution

| # | Gap | Impact | Fix Complexity |
|---|-----|--------|---------------|
| G4 | **ProgramScheduler.start_program() is state-only** — no tokio::spawn, no WASM load/execute | Network-owned programs (reputation scorer, eviction policy, load balancer) never run | MEDIUM — load CID from CraftOBJ, compile, spawn task, add keepalive |

### Priority 3 — Wire protocol deviation

| # | Gap | Impact | Fix Complexity |
|---|-----|--------|---------------|
| G5 | **9-byte wire frame envelope missing** — spec §23 mandates `[type_tag:u32 | version:u8 | payload_len:u32]` header | Interop with future nodes, max message size enforcement, protocol versioning | MEDIUM — wrap postcard payload in frame, parse on receive |

### Priority 4 — Correctness at scale

| # | Gap | Impact | Fix Complexity |
|---|-----|--------|---------------|
| G6 | **No Natural Selection Coordinator** — any holder node may independently coordinate repair | Duplicate repair work, wasted bandwidth | MEDIUM |
| G7 | **No Peer Reputation scoring** (spec §19) | Cannot detect malicious peers, no ban mechanism | MEDIUM |
| G8 | **No HLC (Hybrid Logical Clock)** (spec §42) | No causal ordering of distributed events | MEDIUM |
| G9 | **No Pkarr DNS publication** (spec §37) | Remote nodes cannot discover database root CIDs by name | MEDIUM |
| G10 | **SWIM suspicion timeout differs** (1.5s vs spec's 5s) | More aggressive failure detection, higher false-positive rate | LOW |

---

## Part 6: What Works Well

1. **Architecture matches spec** — initialization order, dependency graph, subsystem boundaries all align with v3.3.
2. **Zero design constraint violations** — CRDT-free, single-writer, raw BLAKE3, filesystem-as-index, kernel scheduler, client-side-only decode.
3. **RLNC implementation is production-quality** — encoder, decoder, recoder all correct with GF(2^8) arithmetic, HomMAC homomorphic property verified.
4. **RPC write security model is correct** — Ed25519 signature verification, CAS conflict detection, non-owner rejection all tested.
5. **Repair pipeline structurally complete** — scanner → mpsc → executor → recode → distribute.
6. **Graceful shutdown is exemplary** — ShutdownSignal event + broadcast channel + node.lock cleanup.
7. **Test coverage is strong** — 31 integration tests covering multi-node scenarios, scale, and subsystem wiring.
8. **Code quality is high** — consistent documentation, proper error handling with anyhow/thiserror, no unsafe code.

---

## Part 7: Recommended Fix Order

1. **G1 (CID-VFS bridge)** — This is the foundation. Until SQLite pages flow through CidVfs, CraftSQL is just an in-memory database with extra steps. Everything else (replication, repair, persistence) depends on this.
2. **G2 (Event bus publishers)** — Simple wiring change. Pass EventBus to CraftOBJ and CidVfs. Publish CidWritten and PageCommitted events.
3. **G3 (RLNC encode on write)** — Subscribe to CidWritten event, encode dirty pages into coded pieces, store pieces in CraftOBJ.
4. **G5 (Wire framing)** — Add 9-byte envelope to send/receive. Enables protocol versioning and max message size enforcement.
5. **G4 (Program execution)** — Implement the tokio::spawn path in ProgramScheduler. Load WASM by CID, execute, keepalive.
6. **G6-G10** — Remaining medium-priority items in any order.
