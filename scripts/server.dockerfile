# Pin both stages to the same Debian release. `rust:slim` floats and
# at the time of writing tracks trixie (Debian 13, glibc 2.41), while
# the runtime image below is bookworm (glibc 2.36). Mixing them links
# the binary against GLIBC_2.38+ symbols the runtime can't satisfy
# ("/lib/x86_64-linux-gnu/libc.so.6: version `GLIBC_2.38' not found").
# Holding the builder on bookworm keeps both sides on the same libc
# floor; bump both lines together if you want a newer Debian later.
FROM rust:slim-trixie AS builder

WORKDIR /app

COPY . .

RUN apt update && apt install -y protobuf-compiler
RUN cargo build --release --package toki-server

FROM debian:13-slim

COPY --from=builder /app/target/release/toki-server /usr/bin/toki-server
RUN  chmod +x /usr/bin/toki-server

COPY ./scripts/server.entrypoint.sh ./entrypoint.sh
RUN  chmod +x ./entrypoint.sh

RUN groupadd -r toki && useradd -r -g toki toki

RUN mkdir -p /data
RUN chown -R toki:toki /data

ENV TOKI_CONFIG=/data/config.toml

USER toki

EXPOSE 50051 50052

ENTRYPOINT ["./entrypoint.sh"]