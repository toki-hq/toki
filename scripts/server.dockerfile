# ── Stage 1: build the admin SPA ──────────────────────────────────
# The React admin UI is embedded into the server binary (rust-embed),
# so its `dist/` must exist before the cargo build. buf is provided via
# npm (the proto is read from crates/proto/proto), so no extra toolchain.
FROM node:20-slim AS ui
WORKDIR /ui
COPY crates/server/admin-ui/package.json crates/server/admin-ui/package-lock.json ./
RUN npm ci
COPY crates/server/admin-ui/ ./
COPY crates/proto/proto /proto/proto
RUN npm run gen && npm run build

# ── Stage 2: build the server ─────────────────────────────────────
# Pin to trixie so the builder's glibc matches the runtime image below.
FROM rust:slim-trixie AS builder

WORKDIR /app

COPY . .
# Drop in the freshly-built SPA so rust-embed bakes the real bundle
# (overwriting the committed placeholder index.html).
COPY --from=ui /ui/dist crates/server/admin-ui/dist

RUN apt update && apt install -y protobuf-compiler
RUN cargo build --release --package toki-server

FROM debian:13-slim

# System CA roots. The ACME client (instant-acme) verifies Let's
# Encrypt's own TLS via the platform trust store; debian-slim ships
# none, so without this issuance fails with "No CA certificates were
# loaded from the system". Tiny, and only needed when [acme] is on.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/toki-server /usr/bin/toki-server
RUN  chmod +x /usr/bin/toki-server

COPY ./scripts/server.entrypoint.sh ./entrypoint.sh
RUN  chmod +x ./entrypoint.sh

RUN groupadd -r toki && useradd -r -g toki toki

RUN mkdir -p /data
RUN chown -R toki:toki /data

ENV TOKI_CONFIG=/data/config.toml

USER toki

# gRPC + audio share port 50051 (TCP for gRPC, UDP for audio — the
# kernel keys binds by `(protocol, port)`, so they coexist). 8000 is
# the admin web panel's default port. 80 + 443 are for Let's Encrypt
# (ACME HTTP-01): when `[acme]` is enabled the operator typically sets
# `[admin].port = 443` and publishes 80 (challenge + HTTP→HTTPS redirect)
# and 443 (panel). Declaring protocols explicitly so `docker run -P` /
# `docker inspect` publish the UDP side too — bare `EXPOSE 50051`
# defaults to TCP only.
EXPOSE 50051/tcp 50051/udp 8000/tcp 80/tcp 443/tcp

ENTRYPOINT ["./entrypoint.sh"]