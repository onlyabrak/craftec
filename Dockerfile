# Craftec Node — Multi-stage Docker build
#
# Build:   docker build -t craftec-node .
# Run:     docker run -e CRAFTEC_DATA_DIR=/data -e CRAFTEC_LISTEN_PORT=4433 craftec-node

# ── Stage 1: Build ───────────────────────────────────────────────────────────

FROM rust:1.83-bookworm AS builder

WORKDIR /build

# Copy workspace manifests first for layer caching.
COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/

# Build in release mode.
RUN cargo build --release --bin craftec-node \
    && strip target/release/craftec-node

# ── Stage 2: Runtime ─────────────────────────────────────────────────────────

FROM debian:bookworm-slim AS runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*

# Create a non-root user.
RUN groupadd -r craftec && useradd -r -g craftec -m craftec

# Copy the built binary.
COPY --from=builder /build/target/release/craftec-node /usr/local/bin/craftec-node

# Data directory (mount as volume for persistence).
RUN mkdir -p /data && chown craftec:craftec /data
VOLUME ["/data"]

# Default environment.
ENV CRAFTEC_DATA_DIR=/data
ENV CRAFTEC_LISTEN_PORT=4433
ENV RUST_LOG=info

# Switch to non-root.
USER craftec

# Expose the default QUIC port.
EXPOSE 4433/udp

ENTRYPOINT ["craftec-node"]
