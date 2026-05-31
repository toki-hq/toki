# Toki

A walkie-talkie style VOIP system. Tune to a frequency, hold a button to talk, release to listen. Pure Rust, end to end — gRPC signaling, encrypted UDP audio, an egui desktop client, and a built-in web admin panel.

## Layout

Cargo workspace:

```
crates/
  proto/    # toki.proto + generated tonic types + UDP wire format
  server/   # gRPC signaling + UDP audio relay + admin web panel
  client/   # egui desktop client
```

## Architecture

- **Signaling** — gRPC over HTTP/2 via [tonic], **always TLS**. Carries registration, frequency join/leave, presence, and PTT events. `Join` opens a server stream that pushes `Event`s (member joined/left, PTT, frequency-changed, display-name-changed) to every member of the caller's current frequency room. Clients hop rooms with `ChangeFrequency` without re-opening the stream.
- **Frequencies as channels** — rooms are addressed by frequency string (e.g. `"446.05"`). The band is 446.00–448.00 MHz in 0.05 MHz steps (41 discrete channels); each is an independent walkie-talkie room with its own member list and single PTT floor.
- **Audio** — raw UDP, out of band, **encrypted + authenticated**. Client→server packets are `[16-byte token][1-byte version][8-byte seq][payload][16-byte tag]`, sealed with ChaCha20-Poly1305 under a per-session key from the TLS handshake. Version `0` is a keepalive (refreshes the NAT mapping / UDP source address, not forwarded); version `1` is a 10 ms raw-PCM frame (mono, i16 LE, 48 kHz); version `2` is a 10 ms **Opus** frame (the default — ~24 kbps audio, ~15–20× smaller than PCM; 10 ms framing keeps mouth-to-ear latency on par with the PCM path). The server is a pure relay — it verifies the tag, enforces a strictly-increasing sequence (replay protection), pins the session to its registering IP, then re-seals the opaque payload to each peer (server→peer packets prepend the codec version so receivers pick the right decoder); it never decodes audio. The operator picks the codec/quality (Raw / Low / Standard / High); the server advertises it at register and clients honor it.
- **Audio I/O** — [cpal] on a dedicated thread, with inline resampling so 44.1/48/96 kHz devices interoperate cleanly. PTT-gated outbound; inbound feeds a latency-managed playback ring that keeps the voice backlog tight (~60 ms target, ~120 ms ceiling) — if playback ever falls behind it skips forward to catch up rather than carrying a growing delay. Output is opened in stereo so the balance control can pan to either ear.
- **Client GUI** — [egui] / eframe radio-strip UI: frequency tuner, live roster with talking indicators, configurable global PTT, memory presets, mic/speaker/balance knobs, roger-beep presets, and a settings panel.
- **Admin panel** — a React (Vite + Tailwind + shadcn/ui) SPA served over HTTPS on a separate port, talking **gRPC-Web** to an embedded `Admin` service. A server-streaming `Watch` RPC drives the live dashboard; unary RPCs handle operator actions (kick / move / rename / priority) and runtime configuration. Two switchable themes (modern + phosphor terminal).

Voice is **Opus** by default (~24 kbps/stream of audio — a ~15–20× cut vs the original raw PCM once per-packet overhead is counted, which scales the whole per-listener fan-out down with it). Frames are 10 ms, matching the capture cadence, so there's no encoder buffering and no added mouth-to-ear delay. Operators can dial quality (Low/Standard/High) or drop to Raw PCM (~780 kbps, no codec) from the admin panel; the choice is advertised to clients at register. The codec lives entirely on the clients — the server relays opaque encrypted payloads — so adding future codecs is a client-only change plus a wire version byte.

## Security

- **TLS is mandatory** for gRPC *and* the admin panel, both served from one hot-swappable cert resolver. Cert source, in precedence order: operator `[tls]` cert/key paths > **`[acme]` Let's Encrypt** (auto-issued + auto-renewed, see [Automatic TLS](#automatic-tls-lets-encrypt)) > an auto-generated self-signed pair under `<data-dir>/tls/`. Renewals hot-swap into both listeners with no restart.
- **Password gate** (optional): clients must echo a shared secret on `Register`. Resolution order is TOML (`password = "…"`) > runtime DB value (set from the admin panel) > open mode. Compared in constant time.
- **UDP audio** is AEAD-encrypted, MAC-verified, replay-protected, and IP-bound — a captured token can't be replayed off-path or from another address.
- **Admin auth**: argon2id password hashes + BLAKE3-hashed, server-revocable session cookies (HttpOnly, Secure, SameSite=Strict). First boot seeds an `admin` user with a random password logged once at `WARN`. Per-IP rate limiting on login; `admin.db` is `chmod 0600`.
- Inbound gRPC messages are capped at 8 KB; a `max_peers` ceiling and an idle reaper bound resource use.
- **Version gate**: clients send their version on `Register`; the server requires a matching **MAJOR.MINOR** (patch may differ — patches are wire-compatible) and rejects a mismatch with `FAILED_PRECONDITION` and a "please update" message. The UDP audio wire format can change across minor versions, so this turns silent dead-air into an actionable error rather than letting incompatible builds half-connect.

## Running

```sh
# server — gRPC TCP :50051, UDP audio :50051 (same number, different
# protocol), admin panel HTTPS :8000 (self-signed by default; see
# "Automatic TLS" for a real Let's Encrypt cert)
cargo run -p toki-server

# client
cargo run -p toki-client
```

### Server environment variables

| Variable | Purpose | Default |
|---|---|---|
| `TOKI_CONFIG` | Path to the optional `config.toml` | `./config.toml` if present |
| `TOKI_DATA_DIR` | Root for TLS certs + `admin.db` (relative paths resolve here) | `.` |
| `TOKI_GRPC_ADDR` | gRPC (TCP) bind address | `0.0.0.0:50051` |
| `TOKI_AUDIO_ADDR` | Audio (UDP) bind address | `0.0.0.0:50051` |
| `TOKI_AUDIO_PUBLIC` | Audio endpoint advertised to clients | the audio bind addr |
| `TOKI_ADMIN_BIND` | Admin panel bind interface | `127.0.0.1` |
| `TOKI_ADMIN_PORT` | Admin panel port | `8000` |
| `TOKI_ADMIN_DB_PATH` | SQLite path for admin users/sessions/config | `admin.db` (under data dir) |
| `TOKI_ADMIN_SESSION_TTL_HOURS` | Admin session lifetime | `12` |
| `TOKI_ADMIN_HTTP_REDIRECT_PORT` | Optional plain-HTTP listener that 308-redirects to HTTPS | unset (disabled) |
| `TOKI_ACME_ENABLED` | Enable Let's Encrypt (ACME HTTP-01) | `false` |
| `TOKI_ACME_DOMAINS` | Comma-separated domain(s) for the cert | unset |
| `TOKI_ACME_EMAIL` | ACME account contact email | unset |
| `TOKI_ACME_STAGING` | Use Let's Encrypt staging (testing) | `false` |
| `TOKI_ACME_TOS_AGREED` | Accept the ACME provider's Terms of Service | `false` |

Anything in the `[tls]`, `[acme]`, `[admin]`, and top-level `password` blocks of `config.toml` is overridden by the matching env var (env > TOML > defaults).

### Docker

```sh
docker pull ellessen/toki-server:latest
# state (certs, admin.db) lives under /data
docker run -p 50051:50051/tcp -p 50051:50051/udp -p 8000:8000 \
  -v toki-data:/data ellessen/toki-server

# With Let's Encrypt: publish 80 (ACME challenge + redirect) and 443
# (admin panel), and set the panel to 443. See "Automatic TLS" below.
docker run -p 50051:50051/tcp -p 50051:50051/udp -p 80:80 -p 443:443 \
  -e TOKI_ACME_ENABLED=true -e TOKI_ACME_DOMAINS=toki.example.com \
  -e TOKI_ACME_EMAIL=ops@example.com -e TOKI_ACME_TOS_AGREED=true \
  -e TOKI_ACME_STAGING=true -e TOKI_ADMIN_PORT=443 \
  -v toki-data:/data ellessen/toki-server
```

### Automatic TLS (Let's Encrypt)

Toki can obtain and renew a browser-trusted certificate via **ACME HTTP-01** — no reverse proxy, no manual cert wrangling. Enable it in `config.toml`:

```toml
[acme]
enabled = true
domains = ["toki.example.com"]      # public DNS name(s) → this server
contact_email = "ops@example.com"
terms_of_service_agreed = true       # required (Let's Encrypt ToS)
staging = true                       # test first; flip to false for a real cert

[admin]
port = 443                           # serve the panel on standard HTTPS
```

**Requirements / how it works:**

- **Public domain + port 80.** HTTP-01 validates on port 80 *only* — Let's Encrypt fetches `http://<domain>/.well-known/acme-challenge/…` during issuance **and every ~60-day renewal**, so port 80 must stay reachable from the internet. Bare IPs can't get a cert.
- **Port topology:** **80** = ACME challenge + 308 redirect to HTTPS; **443** = admin panel (set `[admin].port = 443` so `https://<host>/` works); **50051** = gRPC signaling + UDP audio (client-dialed). Publish 80 + 443 + 50051(tcp+udp).
- **Privileged ports:** binding 80/443 needs root, `CAP_NET_BIND_SERVICE`, or Docker port publishing.
- **System CA roots:** issuance verifies Let's Encrypt's TLS against the OS trust store. The Docker image ships `ca-certificates`; if you run the bare binary on a *minimal* host, make sure a CA bundle is installed (e.g. `apt install ca-certificates`) or issuance fails with "No CA certificates were loaded from the system".
- **Resilient + hot:** the server boots immediately on a self-signed (or cached) cert, obtains the real cert in the background, and **hot-swaps** it into both the gRPC and admin listeners — and again on each renewal — with no restart. The account + issued cert are cached under `<data-dir>/tls/acme/` and reused across restarts.
- **Test with `staging = true` first** (untrusted root, far higher rate limits); once issuance works end-to-end, set `staging = false` for a real, browser-trusted cert.

> Note: the desktop client currently trusts any server cert, so a real cert removes the admin-panel browser warning and is correct PKI hygiene, but client-side validation of the gRPC cert is a separate hardening step.

## Admin panel

Browse to `https://<host>:8000` (self-signed cert → expect a browser warning, or front it with a reverse proxy). Grab the seeded `admin` password from the server's startup log. The panel offers:

- **Live dashboard** (gRPC-Web `Watch` stream) — members per frequency, current PTT holder, session age; updates on a 1 Hz tick and immediately after any admin action.
- **Metrics & KPIs** — time-series charts of voice-relay **bandwidth (ingress/egress)** and **users over time** (selectable 1h / 24h / 7d window), plus uptime / peers / transmitting / busiest-channel KPIs and a host-health card (CPU, memory, disk via `sysinfo`). Samples persist to `admin.db` at 1-minute resolution (7-day retention); the UDP relay's byte counters feed the bandwidth series.
- **Audit log** — persistent record of admin actions (kick / move / rename / priority / channel-name / config / passwords), security events (admin + client auth success/failure), and peer connect/disconnect. Filter by category, page back through history, and export JSONL. Retained 30 days.
- **Operator actions** — kick, move to frequency, rename callsign.
- **Voice priority** — elect a member as a priority speaker on a channel; their PTT preempts a non-priority holder mid-transmission (the cut-off speaker is bumped, the channel hears a distinct priority roger). First-come among priority members.
- **Named channels** — give any frequency a human-readable name (≤16 chars) that clients see beside their tuner. Names persist across emptiness and update live; clear one channel's name or wipe them all. Gated by a Settings toggle (off by default) — while off, clients never receive names and the editor is disabled.
- **Runtime config** — server name, `max_peers`, idle-kick timeout, the named-channels toggle, **voice quality** (Raw PCM / Low / Standard / High Opus), and the gRPC server password, all hot-reloaded without a restart (codec changes apply to clients on their next connect).
- **Account** — change the admin password (revokes other sessions).

## Client features

- **Tuner** — step through the 41-channel band with the ◀ ▶ chevrons; the channel's admin-assigned name scrolls as a marquee in the screen's upper-left when the server has named channels enabled.
- **Memory presets (M1–M4)** — left-click to save/recall a frequency, left-hold to overwrite, right-hold to free; colour-coded (green when you're parked on it, amber when stored).
- **Global hotkeys** (Settings) — PTT (any key or mouse button, default backtick), recall M1–M4, and tune up/down.
- **Knobs** — mic gain, speaker gain, and **balance** (pan received audio + beeps toward one ear for a mono-earpiece feel).
- **Roger beeps** — selectable take-/clear-floor tone presets, with a fixed two-tone cue for priority traffic.
- **Update check** — on launch (and periodically), the client checks GitHub Releases for a newer version and shows an "Update available" pill that opens the download page. Notify-only — it never replaces itself. Toggle and current version live in Settings → Updates.
- Output mute, device pickers, and a connection/event log.

## Trying it locally

Open two clients pointed at one server. **Wear headphones** — both clients use the default mic and speakers, so without headphones you'll get a feedback loop.

```sh
# terminal 1
cargo run -p toki-server

# terminals 2 & 3
cargo run -p toki-client
```

In each client: tune to the same frequency, click Connect, then hold the PTT key (default backtick) — or click-and-hold the on-screen PTT button — to transmit.

## Status

Voice between clients works end to end over encrypted UDP, with TLS signaling, an admin panel, voice priority, and a full radio-strip client. Still ahead: codec (Opus), a real jitter buffer, and NAT-traversal niceties for internet (non-LAN) use.

[tonic]: https://github.com/hyperium/tonic
[egui]: https://github.com/emilk/egui
[cpal]: https://github.com/RustAudio/cpal
[axum]: https://github.com/tokio-rs/axum
