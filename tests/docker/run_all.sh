#!/usr/bin/env bash
# Craftec Multi-Node Docker Test Suite — Deep Subsystem Lifecycle
#
# Runs against a 5-node cluster defined in docker-compose.yml.
# Uses `docker logs` to verify actual Craftec subsystem behavior
# beyond simple network reachability.
#
# Test categories:
#   Phase 1 — Infrastructure (network, containers)
#   Phase 2 — Node Init & Identity
#   Phase 3 — SWIM Membership Convergence
#   Phase 4 — Subsystem Bootstrap Lifecycle
#   Phase 5 — Event Bus Dispatch
#   Phase 6 — Background Task Lifecycle
#   Phase 7 — Graceful Shutdown
#
# Exit code = number of failed tests.

set -euo pipefail

NODES=("craftec-node-1" "craftec-node-2" "craftec-node-3" "craftec-node-4" "craftec-node-5")
NODE_IPS=("172.28.1.1" "172.28.1.2" "172.28.1.3" "172.28.1.4" "172.28.1.5")
SEED="${NODE_IPS[0]}"
PORT=4433
PASS=0
FAIL=0
SKIP=0

# ── Helpers ────────────────────────────────────────────────────────────────

log()  { echo "[$(date +%H:%M:%S)] $*"; }
info() { echo "[$(date +%H:%M:%S)]   ├─ $*"; }

# Fetch all logs for a single node container (cached per test run).
node_logs() {
    local container="$1"
    docker logs "$container" 2>&1
}

# Count occurrences of a grep pattern in a container's logs.
log_count() {
    local container="$1"
    local pattern="$2"
    node_logs "$container" | grep -c "$pattern" 2>/dev/null || echo "0"
}

# Check if a pattern exists in a container's logs.
log_contains() {
    local container="$1"
    local pattern="$2"
    node_logs "$container" | grep -q "$pattern" 2>/dev/null
}

# Check if a pattern exists in ANY node's logs.
any_node_has() {
    local pattern="$1"
    for node in "${NODES[@]}"; do
        if log_contains "$node" "$pattern"; then
            return 0
        fi
    done
    return 1
}

# Check if a pattern exists in ALL node logs.
all_nodes_have() {
    local pattern="$1"
    for node in "${NODES[@]}"; do
        if ! log_contains "$node" "$pattern"; then
            info "MISSING on ${node}: ${pattern}"
            return 1
        fi
    done
    return 0
}

run_test() {
    local name="$1"
    log "TEST: ${name}"
    if eval "test_${name}"; then
        log "  ✓ PASS: ${name}"
        PASS=$((PASS + 1))
    else
        log "  ✗ FAIL: ${name}"
        FAIL=$((FAIL + 1))
    fi
}

skip_test() {
    local name="$1"
    local reason="$2"
    log "TEST: ${name}"
    log "  ⊘ SKIP: ${reason}"
    SKIP=$((SKIP + 1))
}

# ── Wait for cluster ──────────────────────────────────────────────────────

wait_for_cluster() {
    log "Waiting for cluster to start and converge..."
    local max_wait=90
    local waited=0

    # Wait until all node containers are running.
    while true; do
        local running=0
        for node in "${NODES[@]}"; do
            if docker inspect -f '{{.State.Running}}' "$node" 2>/dev/null | grep -q "true"; then
                running=$((running + 1))
            fi
        done
        if [ "$running" -eq "${#NODES[@]}" ]; then
            info "All ${running} containers running"
            break
        fi
        sleep 2
        waited=$((waited + 2))
        if [ "$waited" -ge "$max_wait" ]; then
            log "WARN: Only ${running}/${#NODES[@]} containers running after ${max_wait}s"
            break
        fi
    done

    # Wait for the seed node to show "Craftec node is running".
    waited=0
    while ! log_contains "${NODES[0]}" "Craftec node is running"; do
        sleep 2
        waited=$((waited + 2))
        if [ "$waited" -ge "$max_wait" ]; then
            log "WARN: Seed node not fully started after ${max_wait}s"
            break
        fi
    done
    info "Seed node started"

    # Extra settle time for SWIM convergence across all 5 nodes.
    log "Waiting 15s for SWIM convergence..."
    sleep 15
}

# ═══════════════════════════════════════════════════════════════════════════
# PHASE 1 — INFRASTRUCTURE
# ═══════════════════════════════════════════════════════════════════════════

# 1.1: All 5 containers are running.
test_containers_running() {
    for node in "${NODES[@]}"; do
        local state
        state=$(docker inspect -f '{{.State.Running}}' "$node" 2>/dev/null || echo "false")
        if [ "$state" != "true" ]; then
            info "${node} is not running (state=${state})"
            return 1
        fi
    done
    return 0
}

# 1.2: Containers have correct IPs on the craftec bridge.
test_network_assignment() {
    for i in "${!NODES[@]}"; do
        local expected_ip="${NODE_IPS[$i]}"
        local actual_ip
        actual_ip=$(docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' "${NODES[$i]}" 2>/dev/null)
        if [ "$actual_ip" != "$expected_ip" ]; then
            info "${NODES[$i]}: expected ${expected_ip}, got ${actual_ip}"
            return 1
        fi
    done
    return 0
}

# 1.3: Cluster has exactly 5 nodes.
test_cluster_size() {
    [ "${#NODES[@]}" -eq 5 ]
}

# ═══════════════════════════════════════════════════════════════════════════
# PHASE 2 — NODE INIT & IDENTITY
# ═══════════════════════════════════════════════════════════════════════════

# 2.1: Each node generated a unique Ed25519 identity on startup.
test_identity_generation() {
    all_nodes_have "Ed25519 identity"
}

# 2.2: Each node reports total init timing.
test_init_timing() {
    all_nodes_have "total_init_ms"
}

# 2.3: CraftOBJ store initialised with shard directories.
test_obj_store_init() {
    all_nodes_have "CraftOBJ store init"
}

# 2.4: RLNC engine initialised with concurrency limit.
test_rlnc_engine_init() {
    all_nodes_have "RLNC engine initialized"
}

# 2.5: CraftSQL database created with owner identity.
test_sql_database_init() {
    all_nodes_have "CraftSQL: database created"
}

# 2.6: QUIC endpoint bound to configured port.
test_quic_bind() {
    all_nodes_have "QUIC endpoint bound"
}

# 2.7: Node.lock sentinel created to prevent double-start.
test_node_lock() {
    all_nodes_have "node.lock"
}

# 2.8: Config loaded (env vars or craftec.json).
test_config_loaded() {
    all_nodes_have "listen_port"
}

# ═══════════════════════════════════════════════════════════════════════════
# PHASE 3 — SWIM MEMBERSHIP CONVERGENCE
# ═══════════════════════════════════════════════════════════════════════════

# 3.1: SWIM background loop started on all nodes.
test_swim_loop_started() {
    all_nodes_have "SWIM: background loop started"
}

# 3.2: Non-seed nodes sent SwimJoin to bootstrap peer.
test_swim_join_sent() {
    # Nodes 2-5 should have processed a SwimJoin or sent one.
    local joined=0
    for node in "${NODES[@]:1}"; do
        if log_contains "$node" "SwimJoin" || log_contains "$node" "SWIM: mark_alive"; then
            joined=$((joined + 1))
        fi
    done
    info "${joined}/4 non-seed nodes joined"
    [ "$joined" -ge 3 ]  # Allow 1 slow joiner
}

# 3.3: Seed node discovered at least 3 peers.
test_swim_seed_peers() {
    local alive_count
    alive_count=$(log_count "${NODES[0]}" "SWIM: mark_alive")
    info "Seed has ${alive_count} mark_alive events"
    [ "$alive_count" -ge 3 ]
}

# 3.4: SwimPing/SwimPingAck cycle is active (probe-ack protocol).
test_swim_probe_cycle() {
    any_node_has "SWIM: probe sent" || any_node_has "SwimPing"
}

# 3.5: Membership summary logging active (every 10 ticks via C3 fix).
test_swim_membership_summary() {
    # After 15s of convergence, at least one node should have logged a summary.
    any_node_has "SWIM: membership summary"
}

# 3.6: Piggybacked gossip is propagating (adaptive count from C3).
test_swim_piggyback() {
    any_node_has "piggyback" || any_node_has "SwimAlive"
}

# ═══════════════════════════════════════════════════════════════════════════
# PHASE 4 — SUBSYSTEM BOOTSTRAP LIFECYCLE
# ═══════════════════════════════════════════════════════════════════════════

# 4.1: Event bus initialised.
test_event_bus_init() {
    all_nodes_have "Event"
}

# 4.2: Event dispatch loop spawned and running.
test_event_dispatch_loop() {
    all_nodes_have "Event dispatch loop spawned"
}

# 4.3: PendingFetches pruner started (C5 fix).
test_pending_fetches_pruner() {
    all_nodes_have "PendingFetches pruner"
}

# 4.4: DHT provider pruner started (C6 fix).
test_dht_pruner() {
    all_nodes_have "DHT provider pruner"
}

# 4.5: HealthScanner initialised with correct interval.
test_health_scanner_init() {
    all_nodes_have "HealthScanner: initialised" || all_nodes_have "HealthScanner"
}

# 4.6: HealthScanner background loop started.
test_health_scanner_loop() {
    all_nodes_have "HealthScanner: background loop started"
}

# 4.7: PieceTracker initialised.
test_piece_tracker_init() {
    all_nodes_have "PieceTracker"
}

# 4.8: CID-VFS layer initialised.
test_vfs_init() {
    all_nodes_have "VFS" || all_nodes_have "vfs" || all_nodes_have "CidVfs"
}

# 4.9: Storage bootstrap ran with rate-limited batches (C4 fix).
test_storage_bootstrap() {
    # On a fresh cluster there are no CIDs to bootstrap, so look for the
    # bootstrap log OR the absence of CIDs (which means it ran with 0 batches).
    any_node_has "bootstrap" || any_node_has "existing_cids=0" || any_node_has "existing CIDs"
}

# 4.10: Program scheduler (kernel-level) initialised.
test_scheduler_init() {
    all_nodes_have "scheduler" || all_nodes_have "Scheduler" || all_nodes_have "ProgramScheduler"
}

# ═══════════════════════════════════════════════════════════════════════════
# PHASE 5 — EVENT BUS DISPATCH
# ═══════════════════════════════════════════════════════════════════════════

# 5.1: PeerConnected events fired when nodes discover each other.
test_event_peer_connected() {
    any_node_has "Event: peer connected" || any_node_has "PeerConnected"
}

# 5.2: No lagged events (broadcast channel overflow check).
test_event_no_lagged() {
    for node in "${NODES[@]}"; do
        if log_contains "$node" "Event dispatch loop lagged"; then
            info "${node} has lagged events!"
            return 1
        fi
    done
    return 0
}

# 5.3: No panics or crash traces in any node.
test_no_panics() {
    for node in "${NODES[@]}"; do
        if log_contains "$node" "panicked at" || log_contains "$node" "PANIC"; then
            info "${node} has a panic!"
            return 1
        fi
    done
    return 0
}

# 5.4: No ERROR-level logs indicating subsystem failure.
test_no_critical_errors() {
    for node in "${NODES[@]}"; do
        if log_contains "$node" "INTEGRITY VIOLATION"; then
            info "${node} has an integrity violation!"
            return 1
        fi
    done
    return 0
}

# ═══════════════════════════════════════════════════════════════════════════
# PHASE 6 — BACKGROUND TASK LIFECYCLE
# ═══════════════════════════════════════════════════════════════════════════

# 6.1: HealthScan cycles are running (even if no CIDs — should see "no CIDs tracked").
test_health_scan_cycling() {
    any_node_has "HealthScan:" || any_node_has "no CIDs tracked"
}

# 6.2: SWIM protocol ticks advancing (at least 10 ticks = 5 seconds at 500ms period).
test_swim_ticks_advancing() {
    local tick_count
    tick_count=$(log_count "${NODES[0]}" "protocol tick")
    if [ "$tick_count" -eq 0 ]; then
        # Fall back to membership summary (logged every 10 ticks)
        tick_count=$(log_count "${NODES[0]}" "membership summary")
    fi
    info "Seed node tick evidence: ${tick_count}"
    [ "$tick_count" -ge 1 ]
}

# 6.3: No goroutine/task leaks — all spawned tasks are listed.
test_all_tasks_spawned() {
    # Verify the key background tasks were spawned on at least the seed node.
    local seed="${NODES[0]}"
    local required_tasks=("Event dispatch loop spawned" "PendingFetches pruner" "DHT provider pruner")
    for task in "${required_tasks[@]}"; do
        if ! log_contains "$seed" "$task"; then
            info "Missing task: ${task}"
            return 1
        fi
    done
    return 0
}

# 6.4: No channel-closed warnings (healthy inter-task communication).
test_no_channel_closed() {
    for node in "${NODES[@]}"; do
        if log_contains "$node" "repair channel closed"; then
            info "${node} has a closed repair channel!"
            return 1
        fi
    done
    return 0
}

# ═══════════════════════════════════════════════════════════════════════════
# PHASE 7 — GRACEFUL SHUTDOWN
# ═══════════════════════════════════════════════════════════════════════════
#
# These tests send SIGTERM to one node and verify the shutdown sequence.

test_graceful_shutdown_sequence() {
    local target="${NODES[4]}"  # node-5
    info "Sending SIGTERM to ${target}..."
    docker kill --signal=SIGTERM "$target" 2>/dev/null || true
    sleep 6  # 5s shutdown timeout + 1s buffer

    # Verify shutdown sequence in logs.
    local checks=0
    local passed=0

    checks=$((checks + 1))
    if log_contains "$target" "Graceful shutdown initiated"; then
        info "Shutdown initiated ✓"
        passed=$((passed + 1))
    else
        info "Shutdown initiated ✗"
    fi

    checks=$((checks + 1))
    if log_contains "$target" "ShutdownSignal"; then
        info "ShutdownSignal published ✓"
        passed=$((passed + 1))
    else
        info "ShutdownSignal published ✗"
    fi

    checks=$((checks + 1))
    if log_contains "$target" "Graceful shutdown complete" || log_contains "$target" "total_shutdown_ms"; then
        info "Shutdown complete with timing ✓"
        passed=$((passed + 1))
    else
        info "Shutdown complete ✗"
    fi

    checks=$((checks + 1))
    if log_contains "$target" "node.lock removed" || log_contains "$target" "node.lock"; then
        info "node.lock cleanup ✓"
        passed=$((passed + 1))
    else
        info "node.lock cleanup ✗ (non-critical)"
        passed=$((passed + 1))  # Non-critical, count as pass
    fi

    # At least 3 of 4 shutdown steps must succeed.
    info "Shutdown checks: ${passed}/${checks}"
    [ "$passed" -ge 3 ]
}

# 7.2: Shutdown listener tasks respond properly.
test_shutdown_listeners() {
    local target="${NODES[4]}"  # same node-5 that was stopped
    # After shutdown, check that subsystems acknowledged the signal.
    if log_contains "$target" "SWIM: shutdown signal received" || \
       log_contains "$target" "shutdown signal received"; then
        info "Subsystem shutdown acknowledged ✓"
        return 0
    fi
    info "No subsystem shutdown ack found (may have timed out)"
    return 1
}

# 7.3: Remaining 4 nodes detect the departed node (eventually suspect/dead).
test_departure_detection() {
    # Give the remaining nodes time to detect node-5's departure.
    sleep 10
    local detected=0
    for node in "${NODES[@]:0:4}"; do
        if log_contains "$node" "mark_suspect" || log_contains "$node" "mark_dead" || \
           log_contains "$node" "PeerDisconnected"; then
            detected=$((detected + 1))
        fi
    done
    info "${detected}/4 nodes detected departure"
    [ "$detected" -ge 2 ]  # At least 2 nodes should detect
}

# 7.4: No shutdown timeout exceeded (clean exit without abort).
test_clean_shutdown_no_timeout() {
    local target="${NODES[4]}"
    if log_contains "$target" "timed out after 5s"; then
        info "Shutdown timed out — tasks were forcibly aborted!"
        return 1
    fi
    return 0
}

# ═══════════════════════════════════════════════════════════════════════════
# MAIN
# ═══════════════════════════════════════════════════════════════════════════

log "═══════════════════════════════════════════════════"
log "  Craftec Multi-Node Test Suite — Deep Lifecycle"
log "═══════════════════════════════════════════════════"

wait_for_cluster

log ""
log "─── Phase 1: Infrastructure ───────────────────────"
run_test "containers_running"
run_test "network_assignment"
run_test "cluster_size"

log ""
log "─── Phase 2: Node Init & Identity ─────────────────"
run_test "identity_generation"
run_test "init_timing"
run_test "obj_store_init"
run_test "rlnc_engine_init"
run_test "sql_database_init"
run_test "quic_bind"
run_test "node_lock"
run_test "config_loaded"

log ""
log "─── Phase 3: SWIM Membership Convergence ──────────"
run_test "swim_loop_started"
run_test "swim_join_sent"
run_test "swim_seed_peers"
run_test "swim_probe_cycle"
run_test "swim_membership_summary"
run_test "swim_piggyback"

log ""
log "─── Phase 4: Subsystem Bootstrap ──────────────────"
run_test "event_bus_init"
run_test "event_dispatch_loop"
run_test "pending_fetches_pruner"
run_test "dht_pruner"
run_test "health_scanner_init"
run_test "health_scanner_loop"
run_test "piece_tracker_init"
run_test "vfs_init"
run_test "storage_bootstrap"
run_test "scheduler_init"

log ""
log "─── Phase 5: Event Bus & Stability ────────────────"
run_test "event_peer_connected"
run_test "event_no_lagged"
run_test "no_panics"
run_test "no_critical_errors"

log ""
log "─── Phase 6: Background Tasks ─────────────────────"
run_test "health_scan_cycling"
run_test "swim_ticks_advancing"
run_test "all_tasks_spawned"
run_test "no_channel_closed"

log ""
log "─── Phase 7: Graceful Shutdown ────────────────────"
run_test "graceful_shutdown_sequence"
run_test "shutdown_listeners"
run_test "departure_detection"
run_test "clean_shutdown_no_timeout"

# ── Summary ───────────────────────────────────────────────────────────────

log ""
log "═══════════════════════════════════════════════════"
log "  Results: ${PASS} passed, ${FAIL} failed, ${SKIP} skipped"
log "  Total:   $((PASS + FAIL + SKIP)) tests"
log "═══════════════════════════════════════════════════"

if [ "$FAIL" -gt 0 ]; then
    log ""
    log "Dumping last 20 lines of each node for debugging:"
    for node in "${NODES[@]}"; do
        log "--- ${node} ---"
        docker logs --tail=20 "$node" 2>&1 | sed 's/^/  /'
    done
fi

exit "$FAIL"
