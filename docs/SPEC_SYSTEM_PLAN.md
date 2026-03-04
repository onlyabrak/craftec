# Plan: Machine-Readable Spec Files for All Craftec Subsystems

## Context

AI agents repeatedly drift from confirmed design decisions despite having access to the 1800-line technical foundation doc and memory files. Root causes:
1. Prose docs are too long for reliable context loading and too soft to constrain AI behavior
2. No mechanical enforcement — drift is caught by the user, not by tests
3. Ambiguous field names (e.g., `health_scan_interval_secs`) cause cascading misinterpretation

This plan creates `specs/*.toml` files capturing every system/subsystem's processes, parameters, constraints, state machines, and priorities in a structured format. A `spec_compliance.rs` test suite mechanically asserts code matches spec.

## Deliverables

- **12 spec files** in `specs/` covering all 57 subsystem sections (§1–57)
- **1 spec loader module** (`crates/craftec-tests/src/spec_loader.rs`)
- **1 compliance test file** (`crates/craftec-tests/tests/spec_compliance.rs`) with ~90 assertions
- Sections §58–61 (appendices/roadmap/risks/sources) excluded — reference-only, no constrainable behavior

## TOML Schema

Every spec file follows this consistent structure:

```toml
[meta]
domain       = "rlnc"                    # Machine-readable domain name
title        = "RLNC Erasure Coding"     # Human-readable title
sections     = [4, 28, 29]               # Tech Foundation sections covered
version      = "3.4"

# ── Numeric constants, thresholds, defaults ──────────────────────────────
[[parameters]]
name        = "k_default"
value       = 32
type        = "u32"
unit        = "pieces"
description = "Default generation size for large files"
section     = 4
code_path   = "crates/craftec-types/src/config.rs::NodeConfig.rlnc_k"

# ── Formulas with test cases ─────────────────────────────────────────────
[[formulas]]
name         = "target_piece_count"
expression   = "2 * k + 16"
description  = "Simplifies from k * (2 + 16/k). Do NOT use ceil()."
section      = 4
code_path    = "crates/craftec-health/src/scanner.rs::target_piece_count"
test_cases   = [
  { k = 8, expected = 32 },
  { k = 32, expected = 80 },
]

# ── Hard invariants: never/always rules ──────────────────────────────────
[[constraints]]
name        = "no_network_fetch_for_recode"
rule        = "never"
description = "Recode uses ONLY locally-held pieces. NEVER fetch from network."
section     = 28

# ── Ordered process steps ────────────────────────────────────────────────
[[flows]]
name = "repair"
description = "Health scan → detect → elect → recode → distribute"
section = 55

  [[flows.steps]]
  index       = 1
  name        = "scan"
  action      = "Health scanner evaluates 1% of CIDs"
  precondition = "Node holds >=2 coded pieces for this CID"
  never       = ["Scan CIDs where local piece count < 2"]

# ── Lifecycle state machines ─────────────────────────────────────────────
[[state_machines]]
name = "agent_lifecycle"
initial_state = "Loaded"
  [[state_machines.states]]
  name = "Loaded"
  [[state_machines.transitions]]
  from = "Loaded"
  to = "Running"
  trigger = "scheduler_starts_execution"

# ── Scheduling / ordering ────────────────────────────────────────────────
[[priorities]]
name = "background_jobs"
  [[priorities.items]]
  rank = 1
  name = "repair_recode"
  rationale = "Data durability at risk"

# ── Subsystem dependencies ───────────────────────────────────────────────
[[dependencies]]
from = "health_scanner"
to   = "craftobj_store"
type = "requires"
```

## Spec File Inventory (12 files)

| File | Sections | Key Content |
|------|----------|-------------|
| `specs/architecture.toml` | 1, 2, 10 | Layer model, kernel vs network-owned split, ALPN registry |
| `specs/node_lifecycle.toml` | 11–16 | Init (8-step), startup (6a–6k), shutdown (15-step), crash recovery, config hot-reload, upgrade rules |
| `specs/identity_trust.toml` | 17, 19, 20 | Ed25519 identity, reputation scoring/decay/ban, admission 4-step, subnet diversity |
| `specs/networking.toml` | 3, 18, 21, 22, 25, 26 | SWIM params, bootstrap, connection lifecycle, pool max/turnover, NAT/relay, protocol negotiation |
| `specs/wire_protocol.toml` | 23, 24, 45 | Message type tags, frame layout (9B header), channel capacities, semaphores, rate limits |
| `specs/storage.toml` | 6, 27, 31, 32 | DHT TTL=48h, CraftOBJ Put/Get, bloom FP rate, LRU cache, eviction (5.5min safety), disk watermarks 90/95/99% |
| `specs/rlnc.toml` | 4, 28, 29 | GF(2^8), K values, redundancy formula, target=2k+16, distribution priority, no-piece-indices, no-rarest-first, decode-client-only |
| `specs/health_repair.toml` | 30, 55 | Scan scope (>=2 pieces), 1%/cycle, 300s cycle, Natural Selection top-N election, 1 piece/node/cycle |
| `specs/database.toml` | 5, 33–37 | Page size 16KB, commit flow, snapshot isolation, page cache, root CID publication (5s rate limit, Pkarr TTL) |
| `specs/compute.toml` | 38–41 | WASM fuel/memory limits, agent lifecycle states, host functions, attestation (internal=no DKG, chain-boundary=FROST+DKG), quarantine threshold=10 |
| `specs/coordination.toml` | 42–44, 47–52 | HLC skew ±30s, task scheduler EDF+priorities, batch writer 256/4MB/50ms, event bus capacities, background job priority order, error retry policy |
| `specs/data_flows.toml` | 53, 54, 56, 57 | Write path (11-step sync→async), read path (11-step cache→DHT), attestation path (10-step), join path (12-step) |

## Known Code vs Spec Discrepancies

Found during exploration — spec files will document the authoritative value, compliance tests will catch drift:

| Parameter | Spec | Code | File |
|-----------|------|------|------|
| `max_connections` | 200 (§13, §25) | 256 | `config.rs` |
| `target_piece_count(32)` | 80 (2k+16) | 96 (k×ceil(2+16/k)) | `scanner.rs` |
| `bloom_filter_fp_rate` | 0.001 (§27) | 0.01 | `bloom.rs` |
| `PEER_CONNECTED capacity` | 64 (§52) | 256 | `event.rs` |
| `LRU cache default` | 256 MB (§27) | 1024 entries | `cache.rs` |

These will be flagged by failing spec compliance tests. Code fixes are separate work after spec files land.

## Execution Phases

### Phase 1: Infrastructure
Create the spec loader and test harness skeleton.

**New files:**
- `specs/` directory
- `crates/craftec-tests/src/spec_loader.rs` — TOML deserialization types (`Spec`, `Parameter`, `Formula`, `Constraint`, `Flow`, `StateMachine`, `Priority`, `Dependency`) + `Spec::load()` and `Spec::param()` lookup methods
- `crates/craftec-tests/tests/spec_compliance.rs` — skeleton with `load_spec()` helper

**Modified files:**
- `crates/craftec-tests/Cargo.toml` — add `toml` dependency

**Gate:** `cargo build && cargo test` passes with empty spec directory

### Phase 2: Storage + RLNC + Health/Repair (highest drift-prevention value)
These subsystems have the most documented drift history (MEMORY.md records repeated RLNC corrections).

**New files:**
- `specs/rlnc.toml` — §4, §28, §29
- `specs/health_repair.toml` — §30, §55
- `specs/storage.toml` — §6, §27, §31, §32

**Compliance tests (~25):** K defaults, redundancy formula test cases, target_piece_count, DHT TTL, bloom FP rate, scan cycle, distribution priority constraints

**Gate:** `cargo build && cargo test`

### Phase 3: Networking + Wire Protocol
**New files:**
- `specs/networking.toml` — §3, §18, §21, §22, §25, §26
- `specs/wire_protocol.toml` — §23, §24, §45

**Compliance tests (~20):** SWIM tick/timeout, keepalive, pool max, message type tags, channel capacities, semaphore values

**Gate:** `cargo build && cargo test`

### Phase 4: Node Lifecycle + Identity/Trust
**New files:**
- `specs/node_lifecycle.toml` — §11–16
- `specs/identity_trust.toml` — §17, §19, §20

**Compliance tests (~15):** Startup ordering assertions, config defaults, hot-reload fields, FD limit, key size, reputation initial value

**Gate:** `cargo build && cargo test`

### Phase 5: Database + Compute + Coordination
**New files:**
- `specs/database.toml` — §5, §33–37
- `specs/compute.toml` — §38–41
- `specs/coordination.toml` — §42–44, §47–52

**Compliance tests (~20):** Page size, fuel limits, quarantine threshold, HLC replay window, batch writer triggers, event bus capacities, background job priority

**Gate:** `cargo build && cargo test`

### Phase 6: Architecture + Data Flows
**New files:**
- `specs/architecture.toml` — §1, §2, §10
- `specs/data_flows.toml` — §53, §54, §56, §57

**Compliance tests (~10):** Layer model assertions, data flow step counts and ordering

**Gate:** `cargo build && cargo test`

### Phase 7: Process Integration
Update CLAUDE.md and memory to reference spec files as the canonical source for design-check and context-load workflows. Add `specs/README.md` documenting the schema.

## Verification

After each phase:
1. `cargo build` — compiles
2. `cargo test` — all existing + new compliance tests pass
3. `cargo clippy` — no warnings

After all phases:
4. Grep spec files for completeness: every section §1–57 appears in at least one spec file's `sections` array
5. Run full compliance suite: every `[[parameters]]` entry with a `code_path` has a corresponding test assertion
6. Verify known discrepancies (table above) are flagged by failing tests

## Critical Files Reference

| Purpose | Path |
|---------|------|
| Source of truth for extraction | `docs/craftec_technical_foundation.md` |
| Config defaults to assert | `crates/craftec-types/src/config.rs` |
| RLNC constants | `crates/craftec-types/src/piece.rs` |
| Target piece formula | `crates/craftec-health/src/scanner.rs` |
| SWIM parameters | `crates/craftec-net/src/swim.rs` |
| Event bus capacities | `crates/craftec-types/src/event.rs` |
| Wire message types | `crates/craftec-types/src/wire.rs` |
| Bloom filter config | `crates/craftec-obj/src/bloom.rs` |
| Existing test patterns | `crates/craftec-tests/tests/subsystem_integration.rs` |
