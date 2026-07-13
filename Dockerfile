# syntax=docker/dockerfile:1
#
# Multi-stage build for the KGateway HTTP gateway binary.
# - No libpq / libsqlite needed: sqlx bundles SQLite and uses the pure-Rust Postgres
#   driver; reqwest uses rustls. The runtime image only needs CA certificates for TLS
#   to upstream providers.
# - Runs as a non-root user; config is mounted at /etc/kgateway/config.json.

# ---- builder ----
# rust:1-slim tracks the latest stable 1.x. The workspace's effective MSRV is 1.88
# (transitive deps: home 1.88, icu_* 1.86), above the declared rust-version.
FROM rust:1-slim-bookworm AS builder
WORKDIR /build

# cc/pkg-config cover the handful of build scripts (ring, libsqlite3-sys) that need a C
# toolchain to compile their bundled sources.
RUN apt-get update \
    && apt-get install -y --no-install-recommends pkg-config \
    && rm -rf /var/lib/apt/lists/*

COPY . .
RUN cargo build --release -p kgateway-server \
    && strip target/release/kgateway-server

# ---- runtime ----
FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd -r -u 10001 -m -d /home/kgateway kgateway \
    && mkdir -p /data /etc/kgateway \
    && chown -R kgateway /data /etc/kgateway

COPY --from=builder /build/target/release/kgateway-server /usr/local/bin/kgateway-server

USER kgateway
WORKDIR /home/kgateway
EXPOSE 8080

# Liveness/readiness: the gateway serves GET /health (unauthenticated). Configure an HTTP
# probe against it in your orchestrator (the Helm chart does). No in-image HEALTHCHECK
# because the slim runtime ships no HTTP client.

ENTRYPOINT ["kgateway-server"]
CMD ["--config", "/etc/kgateway/config.json"]
