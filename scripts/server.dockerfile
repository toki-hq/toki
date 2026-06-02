# Build the server with cargo-chef so third-party dependencies compile in
# a *separate, cached* layer ahead of the app code. Without this, the
# single `COPY . . && cargo build` recompiled every dependency on any
# source change. Now the dep layer only busts when Cargo.lock / the proto
# (build-script input) change — app-only edits reuse the cooked deps.
#
# Keep the builder and runtime on the same Debian release (both trixie =
# Debian 13, glibc 2.41). Mixing a newer builder with an older runtime
# links the binary against GLIBC symbols the runtime can't satisfy
# ("version `GLIBC_2.xx' not found"). Bump both `FROM` lines together.

# Base: toolchain + protoc. Also the `target` the docker-compose dev
# service builds (it bind-mounts the source and runs `cargo run`), so keep
# it lean — no cargo-chef here.
FROM rust:slim-trixie AS base
RUN apt update && apt install -y protobuf-compiler
WORKDIR /app

# Chef: base + cargo-chef, shared by the planner & builder stages below.
FROM base AS chef
RUN cargo install cargo-chef --locked

# Planner: distil the dependency graph into a recipe. Runs on the full
# source but emits only recipe.json, so editing app code doesn't change
# this layer's output unless the dependency set actually changed.
FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# Builder: cook the dependencies (the cached layer), then build the app.
FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
# toki-proto's build.rs invokes protoc on these during the deps cook, so
# they must be present before `cargo chef cook` — this small COPY only
# busts the cook layer when the .proto files change.
COPY crates/proto/proto crates/proto/proto
# Scope to the server's dependency graph: cooking the whole workspace
# would drag in the client's eframe/cpal/winit native deps (and their
# system libs, absent here). `--package toki-server` keeps it lean.
RUN cargo chef cook --release --package toki-server --recipe-path recipe.json
# Real source: from here only the workspace crates (toki-proto, toki-
# server) recompile — the cooked third-party deps are reused from target/.
COPY . .
RUN cargo build --release --package toki-server


# Release stage
FROM debian:13-slim AS release
COPY --from=builder /app/target/release/toki-server /usr/bin/toki-server
RUN  chmod +x /usr/bin/toki-server

COPY ./scripts/server.entrypoint.sh ./entrypoint.sh
RUN  chmod +x ./entrypoint.sh

RUN groupadd -r toki && useradd -r -g toki toki

RUN mkdir -p /data
RUN chmod 777 /data

ENV TOKI_CONFIG=/data/config.toml

USER toki

# gRPC + audio share port 50051 (TCP for gRPC, UDP for audio — the
# kernel keys binds by `(protocol, port)`, so they coexist). 8000
# is the admin control-plane. Declaring the protocol explicitly so
# `docker run -P` / `docker inspect` know to publish the UDP side
# as well — bare `EXPOSE 50051` defaults to TCP only.
EXPOSE 50051/tcp 50051/udp 8000/tcp

ENTRYPOINT ["./entrypoint.sh"]
