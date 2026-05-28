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
- **Audio** — raw UDP, out of band. Each client → server packet is `[16-byte session token][1-byte version][payload]`. Version `0` is a keepalive (server learns/refreshes the UDP source address, doesn't forward); version `1` is a 10 ms PCM frame (mono, i16 LE, 48 kHz). The server authenticates by token and relays the payload to every other member of the sender's channels.
- **Audio I/O** — [cpal] on a dedicated thread. Default input/output devices, 48 kHz mono. PTT-gated outbound; inbound is mixed into a shared playback ring with a 500 ms cap to bound latency build-up.
- **Client GUI** — [egui] / eframe. Connection form, channel member list with live talking indicators, PTT button (mouse or SPACE), event log.

Codec is intentionally raw PCM for the foundation (~780 kbps per stream — fine for LAN/broadband). Swapping in Opus is a localized change around `send_audio` / `pcm_from_bytes` and the wire-format version byte.

## Running

```sh
# server (gRPC TCP :50051, UDP audio :50051 — same port, different protocol)
cargo run -p toki-server

# client
cargo run -p toki-client
```

Server env vars: `TOKI_GRPC_ADDR`, `TOKI_AUDIO_ADDR`, `TOKI_AUDIO_PUBLIC`.

## Trying it locally

Open two clients pointed at one server. **Wear headphones** — both clients grab the default microphone and play back through the default speakers, so without headphones you'll get a feedback loop.

```sh
# terminal 1
cargo run -p toki-server

# terminal 2 & 3
cargo run -p toki-client
```

In each client window: enter the same channel name (default `general`), click Connect, hold SPACE (or click and hold the PTT button) to transmit.

## Status

Voice between two clients works end to end. Still ahead: codec (Opus), jitter buffer, server-side GC of dead sessions, multi-channel UI, device pickers, NAT-traversal niceties.

[tonic]: https://github.com/hyperium/tonic
[egui]: https://github.com/emilk/egui
[cpal]: https://github.com/RustAudio/cpal
