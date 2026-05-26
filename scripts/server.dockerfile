FROM rust:slim AS builder

WORKDIR /app

COPY . .

RUN apt update && apt install -y protobuf-compiler
RUN cargo build --release --package toki-server

FROM debian:12-slim

COPY --from=builder /app/target/release/toki-server /usr/bin/toki-server
RUN  chmod +x /usr/bin/toki-server

COPY ./scripts/server.entrypoint.sh ./entrypoint.sh
RUN  chmod +x ./entrypoint.sh

EXPOSE 50051 50052

ENTRYPOINT ["./entrypoint.sh"]