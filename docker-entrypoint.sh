#!/bin/bash
# Craftec Docker entrypoint — handles node ID discovery for QUIC bootstrap.
#
# QUIC/TLS requires knowing the remote node's Ed25519 public key before
# connecting.  In Docker, keys are generated at runtime, so we use a shared
# volume (/bootstrap) to exchange node IDs between containers.
#
# Seed node (CRAFTEC_BOOTSTRAP_PEERS=""):
#   1. Start craftec in the background.
#   2. Wait for node.id to appear (written during init Step 9).
#   3. Copy node.id to /bootstrap/<hostname>.id for other nodes to discover.
#   4. Wait for craftec process.
#
# Non-seed nodes (CRAFTEC_BOOTSTRAP_PEERS="172.28.1.1:4433"):
#   1. Wait for the seed's node.id on the shared /bootstrap volume.
#   2. Construct <hex_node_id>@<ip>:<port> bootstrap peer string.
#   3. Export updated CRAFTEC_BOOTSTRAP_PEERS and exec craftec.

set -e

BOOTSTRAP_DIR="/bootstrap"
DATA_DIR="${CRAFTEC_DATA_DIR:-/data}"

if [ -z "$CRAFTEC_BOOTSTRAP_PEERS" ]; then
    # ── SEED NODE ──────────────────────────────────────────────────────────
    # Start craftec in background, publish node ID, then wait.
    craftec &
    PID=$!

    # Wait for node.id (max 60s).
    for i in $(seq 1 120); do
        if [ -f "$DATA_DIR/node.id" ]; then
            if [ -d "$BOOTSTRAP_DIR" ]; then
                cp "$DATA_DIR/node.id" "$BOOTSTRAP_DIR/$(hostname).id"
                echo "entrypoint: published node ID to $BOOTSTRAP_DIR/$(hostname).id"
            fi
            break
        fi
        sleep 0.5
    done

    wait $PID
else
    # ── NON-SEED NODE ──────────────────────────────────────────────────────
    if [ -d "$BOOTSTRAP_DIR" ]; then
        # Wait for the seed's node ID (max 90s).
        SEED_ID_FILE="$BOOTSTRAP_DIR/node-1.id"
        WAITED=0
        while [ ! -f "$SEED_ID_FILE" ] && [ $WAITED -lt 180 ]; do
            sleep 0.5
            WAITED=$((WAITED + 1))
        done

        if [ -f "$SEED_ID_FILE" ]; then
            SEED_ID=$(cat "$SEED_ID_FILE" | tr -d '[:space:]')
            # Rewrite bootstrap peers: bare ip:port → hex_id@ip:port
            NEW_PEERS=""
            IFS=',' read -ra PEERS <<< "$CRAFTEC_BOOTSTRAP_PEERS"
            for peer in "${PEERS[@]}"; do
                peer=$(echo "$peer" | tr -d '[:space:]')
                [ -z "$peer" ] && continue
                if echo "$peer" | grep -q '@'; then
                    # Already has node ID prefix — keep as-is.
                    NEW_PEERS="${NEW_PEERS}${peer},"
                else
                    NEW_PEERS="${NEW_PEERS}${SEED_ID}@${peer},"
                fi
            done
            export CRAFTEC_BOOTSTRAP_PEERS="${NEW_PEERS%,}"
            echo "entrypoint: bootstrap peers resolved to $CRAFTEC_BOOTSTRAP_PEERS"
        else
            echo "entrypoint: WARNING — seed node ID not found after 90s, starting without bootstrap"
        fi
    fi

    exec craftec
fi
