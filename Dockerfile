# Craftec Node — Multi-stage Docker build
#
# Build:   docker build -t craftec .
# Run:     docker run -e CRAFTEC_DATA_DIR=/data -e CRAFTEC_LISTEN_PORT=4433 craftec

# ── Stage 1: Build ───────────────────────────────────────────────────────────

FROM rust:latest AS builder

WORKDIR /build

# Copy workspace manifests first for layer caching.
COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/

# Build in release mode.
RUN cargo build --release --bin craftec \
    && strip target/release/craftec

# ── Stage 2: Runtime ─────────────────────────────────────────────────────────

FROM debian:bookworm-slim AS runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*

# Create a non-root user.
RUN groupadd -r craftec && useradd -r -g craftec -m craftec

# Copy the built binary and entrypoint script.
COPY --from=builder /build/target/release/craftec /usr/local/bin/craftec
COPY docker-entrypoint.sh /usr/local/bin/docker-entrypoint.sh
RUN chmod +x /usr/local/bin/docker-entrypoint.sh

# Data directory (mount as volume for persistence).
RUN mkdir -p /data /bootstrap && chown -R craftec:craftec /data /bootstrap
VOLUME ["/data"]

# Default environment.
ENV CRAFTEC_DATA_DIR=/data
ENV CRAFTEC_LISTEN_PORT=4433
ENV RUST_LOG=info

# Switch to non-root and set working directory.
USER craftec
WORKDIR /data

# Expose the default QUIC port.
EXPOSE 4433/udp

ENTRYPOINT ["docker-entrypoint.sh"]
