# Toki

A walkie-talkie style VOIP system. Tune to a frequency, hold a button to talk, release to listen. Pure Rust, end to end — gRPC signaling, encrypted UDP audio, an egui desktop client, and a built-in web admin panel.

## Layout

Cargo workspace + a standalone web UI:

```
crates/
  proto/    # toki.proto + generated tonic types + UDP wire format
  server/   # gRPC signaling + UDP audio relay + admin gRPC-Web service
  client/   # egui desktop client
admin-ui/   # React/Vite admin SPA (JS/TS, not a cargo crate) — its own
            # Docker image; reverse-proxies to the server admin port
```

## Architecture

- **Signaling** — gRPC over HTTP/2 via [tonic], **always TLS**. Carries registration, frequency join/leave, presence, and PTT events. `Join` opens a server stream that pushes `Event`s (member joined/left, PTT, frequency-changed, display-name-changed) to every member of the caller's current frequency room. Clients hop rooms with `ChangeFrequency` without re-opening the stream.
- **Frequencies as channels** — rooms are addressed by frequency string (e.g. `"446.05"`). The band is 446.00–448.00 MHz in 0.05 MHz steps (41 discrete channels); each is an independent walkie-talkie room with its own member list and single PTT floor.
- **Audio** — raw UDP, out of band, **encrypted + authenticated**. Client→server packets are `[16-byte token][1-byte version][8-byte seq][payload][16-byte tag]`, sealed with ChaCha20-Poly1305 under a per-session key from the TLS handshake. Version `0` is a keepalive (refreshes the NAT mapping / UDP source address, not forwarded); version `1` is a 10 ms raw-PCM frame (mono, i16 LE, 48 kHz); version `2` is a 10 ms **Opus** frame (the default — ~24 kbps audio, ~15–20× smaller than PCM; 10 ms framing keeps mouth-to-ear latency on par with the PCM path). The server is a pure relay — it verifies the tag, enforces a strictly-increasing sequence (replay protection), pins the session to its registering IP, then re-seals the opaque payload to each peer (server→peer packets prepend the codec version so receivers pick the right decoder); it never decodes audio. The operator picks the codec/quality (Raw / Low / Standard / High); the server advertises it at register and clients honor it.
- **Audio I/O** — [cpal] on a dedicated thread, with inline resampling so 44.1/48/96 kHz devices interoperate cleanly. PTT-gated outbound; inbound feeds a latency-managed playback ring that keeps the voice backlog tight (~60 ms target, ~120 ms ceiling) — if playback ever falls behind it skips forward to catch up rather than carrying a growing delay. Output is opened in stereo so the balance control can pan to either ear.
- **Client GUI** — [egui] / eframe radio-strip UI: frequency tuner, live roster with talking indicators, configurable global PTT, memory presets, mic/speaker/balance knobs, roger-beep presets, and a settings panel.
- **Admin panel** — a React (Vite + Tailwind + shadcn/ui) SPA that talks **gRPC-Web** to the server's embedded `Admin` service. It ships as its **own** container (`admin-ui/`): an nginx image that serves the static SPA and reverse-proxies the gRPC-Web + `/api` cookie endpoints to the server's admin port, so the browser stays same-origin and the `HttpOnly + Secure + SameSite=Strict` session cookie round-trips. A server-streaming `Watch` RPC drives the live dashboard; unary RPCs handle operator actions (kick / move / rename / priority / mute / channel-mute / ban) and runtime configuration. Two switchable themes (modern + phosphor terminal).

Voice is **Opus** by default (~24 kbps/stream of audio — a ~15–20× cut vs the original raw PCM once per-packet overhead is counted, which scales the whole per-listener fan-out down with it). Frames are 10 ms, matching the capture cadence, so there's no encoder buffering and no added mouth-to-ear delay. Operators can dial quality (Low/Standard/High) or drop to Raw PCM (~780 kbps, no codec) from the admin panel; the choice is advertised to clients at register. The codec lives entirely on the clients — the server relays opaque encrypted payloads — so adding future codecs is a client-only change plus a wire version byte.

## Security

- **TLS is mandatory** for gRPC. The operator supplies a cert + key in `[tls]`, or the server auto-generates a self-signed pair under `<data-dir>/tls/` on first run and reuses it.
- **Password gate** (optional): clients must echo a shared secret on `Register`. Resolution order is TOML (`password = "…"`) > runtime DB value (set from the admin panel) > open mode. Compared in constant time.
- **UDP audio** is AEAD-encrypted, MAC-verified, replay-protected, and IP-bound — a captured token can't be replayed off-path or from another address.
- **Admin auth**: argon2id password hashes + BLAKE3-hashed, server-revocable session cookies (HttpOnly, Secure, SameSite=Strict). First boot seeds an `admin` user with a random password logged once at `WARN`. Per-IP rate limiting on login; `admin.db` is `chmod 0600`.
- **Client identity** (optional, on by default): each client mints a persistent **ed25519 keypair** on first run; the public key *is* the identity, shown as its 8-char fingerprint (e.g. `7Q4XF9KB`) in the admin panel and audit log. At register the client signs a server-issued challenge, so an identity seen on screen can't be impersonated. The machine identifier travels only as a **salted BLAKE3 hash** (never the raw MAC/machine-id), letting an operator correlate "fresh identity, same machine" after a config wipe without learning the hardware address. Identity-less clients still connect; a present-but-invalid identity is rejected, never silently downgraded.
- **Identity bans**: admins ban an identity (optionally its machine hash too — a wiped config stays banned) from the panel; the session is kicked and future registers are rejected with the operator's reason. Bans persist in the admin DB and are reviewed/lifted in the panel's Bans view; rejected attempts land in the audit log. An optional **require-identity** server setting rejects identity-less registers entirely, closing the anonymous-evader hole.
- Inbound gRPC messages are capped at 8 KB; a `max_peers` ceiling and an idle reaper bound resource use.
- **Version gate**: clients send their version on `Register`; the server requires a matching **MAJOR.MINOR** (patch may differ — patches are wire-compatible) and rejects a mismatch with `FAILED_PRECONDITION` and a "please update" message. The UDP audio wire format can change across minor versions, so this turns silent dead-air into an actionable error rather than letting incompatible builds half-connect.

## Documentation

A full operator/user guide — install, config, env vars, admin panel, recipes, troubleshooting, architecture — lives at [docs/USER_GUIDE.md](docs/USER_GUIDE.md).

## Running

```sh
# server — gRPC TCP :50051, UDP audio :50051 (same number, different
# protocol), admin control-plane HTTPS :8000
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
| `TOKI_ADMIN_DB_URL` | Admin store connection URL (overrides `DB_PATH`; selects backend) | unset → SQLite from `DB_PATH` |
| `TOKI_ADMIN_SESSION_TTL_HOURS` | Admin session lifetime | `12` |
| `TOKI_ADMIN_HTTP_REDIRECT_PORT` | Optional plain-HTTP listener that 308-redirects to HTTPS | unset (disabled) |

Anything in the `[tls]`, `[admin]`, and top-level `password` blocks of `config.toml` is overridden by the matching env var (env > TOML > defaults).

### Admin UI environment variables

The admin SPA is a separate service (`admin-ui/`). Its only setting is the backend it proxies to:

| Variable | Purpose | Default |
|---|---|---|
| `TOKI_SERVER_GRPC_ENDPOINT` | The server **admin port** the UI reverse-proxies `/api/*` + the gRPC-Web `Admin` service to (it co-serves both). Read at container start (nginx) or by the Vite dev server — runtime, not baked into the bundle. | `https://toki-server:8000` |

### Admin database backends

The admin store (users, sessions, runtime config, channel names, metrics, audit log) runs on **SQLite by default** — zero-config, embedded, a single `admin.db` file. To use a **remote MariaDB/MySQL or PostgreSQL** server instead, set a connection URL; the backend is chosen by the scheme:

```toml
[admin]
# SQLite (default — omit to use the db_path shorthand):
# database_url = "sqlite:///var/lib/toki/admin.db?mode=rwc"
database_url = "postgres://toki:secret@db.example.com/toki"   # or
# database_url = "mysql://toki:secret@db.example.com/toki"    # MariaDB/MySQL (mariadb:// also works)
```

or via env: `TOKI_ADMIN_DB_URL=postgres://toki:secret@db.example.com/toki`.

- Remote backends connect over **TLS** (rustls/ring — no OpenSSL) when the server requires it; the password is redacted from startup logs.
- **Startup connection retry:** the initial connect to a remote backend retries with exponential backoff (~0.5s → 5s, up to ~60s total) so a DB container that's still starting (docker-compose `depends_on` / k8s ordering) gets time to come up instead of failing the boot. A genuine misconfiguration (bad host/credentials) still surfaces as a clear startup error once the budget is spent.
- **Fresh start per backend:** pointing at MariaDB/Postgres creates the schema and re-seeds the `admin` user (password logged once) — it does **not** copy an existing `admin.db`. SQLite files keep working unchanged, including the legacy column auto-upgrade.
- The embedded SQLite driver is statically linked, so the default build stays a self-contained binary.

### Docker

The server and the admin UI are **two images**. Run the server alone if you don't need the panel:

```sh
docker pull ellessen/toki-server:latest
# state (certs, admin.db) lives under /data
docker run -p 50051:50051/tcp -p 50051:50051/udp -p 8000:8000 \
  -v toki-data:/data ellessen/toki-server
```

Add the admin UI as a second service pointing at the server's admin port:

```sh
docker pull ellessen/toki-admin-ui:latest
docker run -p 8080:80 \
  -e TOKI_SERVER_GRPC_ENDPOINT=https://<server-host>:8000 \
  ellessen/toki-admin-ui
```

The bundled `docker-compose.yml` wires both (plus a Postgres) together for a local stack: `docker compose up`. The UI image serves plain HTTP on `:80` — terminate TLS in front of it (Coolify / Traefik / etc.); the `Secure` session cookie requires the browser to reach the UI over HTTPS.

## Admin panel

The panel is the **`toki-admin-ui` service**, not the server. Browse to it (e.g. `https://<ui-host>/`); it reverse-proxies to the server's admin port, so the browser only ever sees the UI origin and the session cookie stays same-origin. For local UI development, run it against a server directly:

```sh
cd admin-ui
TOKI_SERVER_GRPC_ENDPOINT=https://localhost:8000 npm run dev   # https://localhost:5173
```

Grab the seeded `admin` password from the server's startup log. The panel offers:

- **Live dashboard** (gRPC-Web `Watch` stream) — members per frequency, current PTT holder, session age; updates on a 1 Hz tick and immediately after any admin action.
- **Metrics & KPIs** — time-series charts of voice-relay **bandwidth (ingress/egress)** and **users over time** (selectable 1h / 24h / 7d window), plus uptime / peers / transmitting / busiest-channel KPIs and a host-health card (CPU, memory, disk via `sysinfo`). Samples persist to `admin.db` at 1-minute resolution (7-day retention); the UDP relay's byte counters feed the bandwidth series.
- **Audit log** — persistent record of admin actions (kick / move / rename / priority / mute / channel-mute / channel-name / config / passwords), security events (admin + client auth success/failure), and peer connect/disconnect. Filter by category, page back through history, and export JSONL. Retained 30 days.
- **Operator actions** — kick, move to frequency, rename callsign.
- **Server-side mute** — silence a member's transmit without disconnecting them: their PTT presses are refused server-side (they stay connected and keep hearing the channel). The gentle lever between nothing and kick/ban. Muting the current floor-holder drops the floor on the spot; the muted client's PTT button goes red ("UNABLE TO TALK") and stops uploading. Session-scoped, audited, and enforced by a relay-side speak-gate the channel mute below shares.
- **Channel mute / No-Talk channels** — silence a *whole frequency*: while muted, no one tuned there can transmit (moving to another channel restores it instantly) — *except* **priority speakers**, who keep their voice. That's the "stage" / "town-hall" model: a default-muted channel where the operator grants voice by promoting a member to priority speaker. Same relay speak-gate as member mute, persisted across restarts and occupancy; muting drops any non-priority in-progress floor. Toggle per-channel from the panel; a MUTED badge flags it (occupied or not). An individual member mute still outranks a priority grant.
- **Voice priority** — elect a member as a priority speaker on a channel; their PTT preempts a non-priority holder mid-transmission (the cut-off speaker is bumped, the channel hears a distinct priority roger). First-come among priority members.
- **Connection-quality telemetry** — each client measures its own link health — round-trip time (a timestamped probe on the UDP keepalive that the server bounces back as a `PONG`), inter-arrival jitter, and packet loss (gaps in the server→client sequence) — and shows it as 4 signal bars on the radio strip (green / amber / red, raw numbers on hover). Clients report the metrics up to the server, so the admin Rooms view carries a per-member `RTT · loss` column. Answers "why am I choppy?" at a glance.
- **Named channels** — give any frequency a human-readable name (≤16 chars) that clients see beside their tuner. Names persist across emptiness and update live; clear one channel's name or wipe them all. Gated by a Settings toggle (off by default) — while off, clients never receive names and the editor is disabled.
- **Runtime config** — server name, `max_peers`, idle-kick timeout, the named-channels toggle, **voice quality** (Raw PCM / Low / Standard / High Opus), and the gRPC server password, all hot-reloaded without a restart (codec changes apply to clients on their next connect).
- **Account** — change the admin password (revokes other sessions).

## Client features

- **Tuner** — step through the 41-channel band with the ◀ ▶ chevrons; the channel's admin-assigned name scrolls as a marquee in the screen's upper-left when the server has named channels enabled.
- **Memory presets (M1–M4)** — left-click to save/recall a frequency, left-hold to overwrite, right-hold to free; colour-coded (green when you're parked on it, amber when stored).
- **Global hotkeys** (Settings) — bind PTT, the M1–M4 recalls, and tune up/down to *any* peripheral: keyboard, mouse, game controller/joystick, Elgato Stream Deck, or generic USB HID. PTT (default backtick) also takes an optional secondary/fallback binding. Keyboard/mouse stay passthrough; other devices are capture-only.
- **Knobs** — mic gain, speaker gain, and **balance** (pan received audio + beeps toward one ear for a mono-earpiece feel).
- **Voice DSP** — capture-side **noise suppression** (RNNoise, pure Rust) + **automatic gain control**, applied to the mic signal before Opus encode so the cleanup reaches every listener. Speech-gated AGC (≈ −18 dBFS target, fast-down/slow-up, peak-limited) won't pump the noise floor during pauses. Both stages on by default and individually toggleable live in Settings → Voice DSP — both off is a bit-exact raw mic for the unprocessed CB character.
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
