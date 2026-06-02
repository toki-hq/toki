# Pin both stages to the same Debian release. `rust:slim` floats and
# at the time of writing tracks trixie (Debian 13, glibc 2.41), while
# the runtime image below is bookworm (glibc 2.36). Mixing them links
# the binary against GLIBC_2.38+ symbols the runtime can't satisfy
# ("/lib/x86_64-linux-gnu/libc.so.6: version `GLIBC_2.38' not found").
# Holding the builder on bookworm keeps both sides on the same libc
# floor; bump both lines together if you want a newer Debian later.

# Base stage
FROM rust:slim-trixie AS base

RUN apt update && apt install -y protobuf-compiler

# Builder stage
#
# The admin SPA is no longer embedded into the binary — it ships as the
# standalone toki-admin-ui image (scripts/admin-ui.dockerfile). So this is
# a pure Rust build; no Node toolchain required.
FROM base AS builder

WORKDIR /app
COPY . .
RUN cargo build --release --package toki-server


# Release stage
FROM debian:13-slim as release
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
# is the admin web panel. Declaring the protocol explicitly so
# `docker run -P` / `docker inspect` know to publish the UDP side
# as well — bare `EXPOSE 50051` defaults to TCP only.
EXPOSE 50051/tcp 50051/udp 8000/tcp

ENTRYPOINT ["./entrypoint.sh"]