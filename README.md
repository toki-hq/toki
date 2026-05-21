# Toki

A walkie-talkie style VOIP client. Hold a button, talk; release, listen. Pure Rust, end-to-end.

## Layout

Cargo workspace:

```
crates/
  proto/    # toki.proto + generated tonic types
  server/   # gRPC signaling + UDP audio relay
  client/   # egui desktop client
```

## Architecture

- **Signaling** — gRPC over HTTP/2 via [tonic]. Carries registration, channel join/leave, presence, and PTT key events. The `JoinChannel` server stream pushes `ChannelEvent`s to every connected member.
- **Audio** — raw UDP, out of band. Each packet is `[16-byte session token][1-byte version][opus frame]`. The server identifies the sender by token, learns the client's public address on first packet, and relays the payload to every other member of every channel the sender is in.
- **Client GUI** — [egui] / eframe. Single window: server URL, display name, channel, PTT button, event log.

A codec (Opus is the obvious pick) and capture/playback (cpal) aren't wired up yet — that's the next slice.

## Running

```sh
# server (gRPC :50051, UDP audio :50052)
cargo run -p toki-server

# client
cargo run -p toki-client
```

Server env vars: `TOKI_GRPC_ADDR`, `TOKI_AUDIO_ADDR`, `TOKI_AUDIO_PUBLIC`.

## Status

Scaffolding only — wire protocol shaped, GUI stub, no audio I/O yet.

[tonic]: https://github.com/hyperium/tonic
[egui]: https://github.com/emilk/egui
