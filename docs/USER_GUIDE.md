# Toki — User Guide

## Overview

Toki is a walkie-talkie style VOIP application written in pure Rust. Each
participant holds a key to talk and releases to listen — a half-duplex radio,
not a conference call. Rooms are addressed by *frequency* (e.g. `446.05`), so a
group can carve up channels the way a hardware PMR radio would.

The project ships three deliverables:

- **`toki-server`** — gRPC signaling + encrypted UDP audio relay + an embedded
  gRPC-Web admin control-plane service.
- **`toki`** — the egui desktop client.
- **`admin-ui`** — a standalone React/Vite web panel (its own Docker image) that
  reverse-proxies to the server's admin port.

Voice is **Opus** by default (~24 kbps/stream), with a raw-PCM fallback. It
targets small, self-hosted deployments — a team chat for a LAN, a community VPS,
a home lab.

## Prerequisites

- **Rust toolchain** — stable, edition 2021 or newer. Install via [rustup](https://rustup.rs/).
- **Protobuf compiler** (`protoc`) — required to build `toki-proto`.
  - Debian/Ubuntu: `sudo apt install protobuf-compiler`
  - macOS (Homebrew): `brew install protobuf`
  - Windows: download a release from [protocolbuffers/protobuf](https://github.com/protocolbuffers/protobuf/releases) and put `protoc.exe` on `PATH`.
- **Linux client build deps** — egui + cpal need X11, GL, ALSA, and Opus
  development packages:
  ```sh
  sudo apt install libasound2-dev libgl1-mesa-dev libx11-dev \
                   libxcursor-dev libxi-dev libxkbcommon-dev \
                   libxkbcommon-x11-dev libxcb1-dev \
                   libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev \
                   libopus-dev libudev-dev
  ```
  (`libudev-dev` is needed by the client's HID input backend.)
- **Node.js 24** — only needed to build the standalone admin UI (`admin-ui/`).
  Not required to build or run the server/client.
- **Headphones** — both client and host grab the system default microphone and
  speakers. Without headphones you'll get a feedback loop when testing on a
  single machine.
- **Open ports** (server side):
  - `50051/tcp` — gRPC signaling (default)
  - `50051/udp` — audio relay (default; shares the port number with gRPC because TCP and UDP are separate kernel binding tuples)
  - `8000/tcp` — admin control-plane (default, loopback-only out of the box)

## Installation

### From source (workspace build)

```sh
git clone https://github.com/toki-hq/toki.git
cd toki
cargo build --release
```

This produces:

- `target/release/toki-server` — the server binary
- `target/release/toki` — the desktop client binary (the binary name is `toki`, the package is `toki-client`)

### Server in Docker

A Dockerfile is included at [scripts/server.dockerfile](../scripts/server.dockerfile):

```sh
docker pull ellessen/toki-server:latest
docker run --rm \
  -p 50051:50051/tcp -p 50051:50051/udp -p 8000:8000/tcp \
  -v $(pwd)/data:/data \
  ellessen/toki-server
```

The entrypoint sets `TOKI_DATA_DIR=/data`, so the auto-generated TLS cert pair,
admin store, and (optionally) `config.toml` all land under that volume.

### Admin UI

The web panel is a separate service. Pull its image and point it at the
server's admin port:

```sh
docker pull ellessen/toki-admin-ui:latest
docker run --rm -p 8080:80 \
  -e TOKI_SERVER_GRPC_ENDPOINT=https://<server-host>:8000 \
  ellessen/toki-admin-ui
```

Or build/run it from source for development:

```sh
cd admin-ui
npm ci
TOKI_SERVER_GRPC_ENDPOINT=https://localhost:8000 npm run dev   # https://localhost:5173
```

The UI image serves plain HTTP on `:80` and reverse-proxies the gRPC-Web +
`/api` cookie endpoints to the server, so the browser stays same-origin and the
admin session cookie round-trips. Terminate TLS in front of it (Coolify /
Traefik / etc.) — the `Secure` cookie requires the browser to reach the UI over
HTTPS.

### Client as a desktop bundle (macOS)

The client crate ships `cargo-bundle` metadata. From the repo root:

```sh
cargo install cargo-bundle
cargo bundle -p toki-client --release
# Output: target/release/bundle/osx/Toki.app
```

Release builds for macOS (`.app`), Windows (`.exe`), and Linux are also attached
to each [GitHub Release](https://github.com/toki-hq/toki/releases).

## Quick Start

Run a server and two clients on one machine to verify everything works.

```sh
# Terminal 1 — server (gRPC TCP :50051, UDP audio :50051, admin HTTPS :8000)
cargo run -p toki-server

# Terminal 2 — first client
cargo run -p toki-client

# Terminal 3 — second client
cargo run -p toki-client
```

In each client window:

1. Enter the server host (`127.0.0.1`) and port (`50051`).
2. Pick a display name (default `anon`).
3. Set the frequency to the same value on both clients (default `447.00`).
4. Click **Connect**.
5. **Wear headphones**, then hold the backtick key (`` ` ``) or click and hold the on-screen PTT button to transmit.

On first server boot you'll see a `WARN`-level log line printing the seeded
admin username (`admin`) and a random password — copy these out of the journal.
To reach the panel, run the `admin-ui` service (see above) pointed at
`https://127.0.0.1:8000` and browse to it. The server's TLS cert is self-signed;
the UI's upstream hop trusts it automatically.

## Configuration

### Server: `config.toml`

The server loads its config from (in order of precedence):

1. `$TOKI_CONFIG` — full path to a TOML file.
2. `./config.toml` next to the working directory.
3. Built-in defaults (open mode, no password, admin on `127.0.0.1:8000`).

A *missing* file silently falls back to defaults. A *malformed* file is fatal —
the server refuses to boot so a TOML typo can't accidentally disarm the
password gate.

Example `config.toml`:

```toml
# Shared-secret password clients must supply at registration.
# Omit or leave empty for open mode (no password).
password = "hunter2"

# Optional. Omit to let the server auto-generate a self-signed cert.
[tls]
cert = "/etc/letsencrypt/live/toki.example.com/fullchain.pem"
key  = "/etc/letsencrypt/live/toki.example.com/privkey.pem"

# All admin keys are optional; defaults shown below.
[admin]
bind              = "127.0.0.1"
port              = 8000
db_path           = "admin.db"
session_ttl_hours = 12
# database_url = "postgres://toki:secret@db.example.com/toki"  # remote backend
# http_redirect_port = 8080   # arm a plain-HTTP → HTTPS 308 redirect listener
```

#### Server environment variables

| Variable | Type | Default | Description |
|---|---|---|---|
| `TOKI_CONFIG` | path | unset | Override the TOML config path. |
| `TOKI_DATA_DIR` | path | `.` | Runtime data root. Auto-generated TLS files land in `{TOKI_DATA_DIR}/tls/`; a relative `[admin] db_path` is resolved here. Absolute operator paths are honored verbatim. |
| `TOKI_GRPC_ADDR` | `host:port` | `0.0.0.0:50051` | TCP bind for the gRPC signaling channel. |
| `TOKI_AUDIO_ADDR` | `host:port` | `0.0.0.0:50051` | UDP bind for the audio relay. Shares the port with gRPC by default; kernel binds are keyed by `(protocol, port)`. |
| `TOKI_AUDIO_PUBLIC` | `host:port` | value of `TOKI_AUDIO_ADDR` | UDP endpoint advertised to clients in `RegisterResponse`. Set this when behind NAT/port-forwarding (e.g. `203.0.113.5:50051`). **Only the host is used by the client** — it always connects UDP on the same port it reached gRPC on (audio and gRPC share a port number by design), so the advertised port here is informational. Run audio on the gRPC port; don't try to split them via this var. |
| `TOKI_ADMIN_BIND` | string | from TOML (`127.0.0.1`) | Override the admin bind interface. |
| `TOKI_ADMIN_PORT` | u16 | from TOML (`8000`) | Override the admin port. |
| `TOKI_ADMIN_DB_PATH` | path | from TOML (`admin.db`) | Override the SQLite admin store path. |
| `TOKI_ADMIN_DB_URL` | URL | unset | Admin store connection URL; overrides `DB_PATH` and selects the backend (SQLite / MariaDB / PostgreSQL). |
| `TOKI_ADMIN_SESSION_TTL_HOURS` | u64 | from TOML (`12`) | Override the admin session cookie TTL. |
| `TOKI_ADMIN_HTTP_REDIRECT_PORT` | u16 or empty | from TOML (unset) | Arm a plain-HTTP listener on this port that 308-redirects to the HTTPS admin. Empty string explicitly disables. |
| `RUST_LOG` | tracing filter | `info` | Standard `tracing-subscriber` filter, e.g. `RUST_LOG=toki_server=debug`. |

Precedence: **env vars > TOML > built-in defaults**.

#### Admin UI environment variables

The standalone panel has a single setting — the backend it proxies to:

| Variable | Default | Description |
|---|---|---|
| `TOKI_SERVER_GRPC_ENDPOINT` | `https://toki-server:8000` | The server **admin port** the UI reverse-proxies `/api/*` + the gRPC-Web `Admin` service to. Read at container start (nginx) or by the Vite dev server — runtime, not baked into the bundle. Format: `scheme://host:port`, no trailing path. Changing it takes effect on a container restart/redeploy. |

#### Admin database backends

The admin store (users, sessions, runtime config, channel names, metrics, audit
log) runs on **SQLite by default** — zero-config, a single `admin.db` file. To
use a **remote MariaDB/MySQL or PostgreSQL** server, set a connection URL (the
scheme selects the backend):

```toml
[admin]
database_url = "postgres://toki:secret@db.example.com/toki"   # or
# database_url = "mysql://toki:secret@db.example.com/toki"     # MariaDB/MySQL
```

or `TOKI_ADMIN_DB_URL=postgres://toki:secret@db.example.com/toki`.

- Remote backends connect over **TLS** (rustls/ring — no OpenSSL); the password
  is redacted from startup logs.
- **Startup retry:** the initial connect retries with exponential backoff
  (~0.5 s → 5 s, up to ~60 s total), so a DB container still coming up (compose
  `depends_on` / k8s ordering) gets time instead of failing the boot.
- **Fresh start per backend:** pointing at MariaDB/Postgres creates the schema
  and re-seeds the `admin` user — it does **not** copy an existing `admin.db`.

#### Runtime-mutable server settings (admin panel)

A second tier of settings lives in the admin store and is editable from the web
panel without restarting the server. They start at their built-in defaults on
first boot:

| Field | Default | Description |
|---|---|---|
| `serverName` | `""` | Display name shown in the admin panel header and to clients. |
| `maxPeers` | `256` | Hard ceiling on simultaneously-registered clients. Excess registrations are rejected with `RESOURCE_EXHAUSTED`. |
| `idleKickSecs` | `10` | Stale-client eviction threshold. A client with no inbound UDP packet for this long is removed and its peers are notified. |
| `voiceQuality` | `Standard` | Codec/quality advertised to clients at register: Raw PCM, or Low/Standard/High Opus. |
| `namedChannels` | `off` | When on, clients receive admin-assigned channel names beside their tuner. |
| `uniqueCallsigns` | `on` | When on, a register or admin rename onto a callsign another connected member already holds is refused (`ALREADY_EXISTS`, case-insensitive). A name frees the instant its holder disconnects; a keypair client reconnecting keeps its own name. Off allows duplicates. |
| `opusFrameMs` | `10` | Opus encoder frame duration advertised to clients at register: `10`, `20`, or `40` ms. Larger frames bundle more 10 ms capture chunks per UDP packet, reducing per-packet overhead on the O(N) relay fan-out — useful when per-packet overhead dominates payload (~53 B overhead vs ~30 B payload at 24 kbps/10 ms). Cost: +10 ms latency (20 ms) or +30 ms latency (40 ms) mouth-to-ear. Stacks with DTX. No effect when `voiceQuality` is Raw PCM. **Compatibility:** `opusFrameMs > 10` requires all clients to be on the release that introduced this setting or newer — older clients have a decode buffer sized for ≤30 ms (1440 samples) and would overflow on a 40 ms frame (1920 samples). |
| `grpcPassword` | `""` | Runtime-editable shared secret. **Overridden** by `config.toml`'s `password` field — if TOML set one, the UI greys out this input. |

#### Global Broadcast

Global broadcast lets a designated client transmit simultaneously to every
frequency room that currently has listeners — a server-wide all-hands
announcement that cuts through every channel at once.

**Granting and revoking**

In the admin panel **Channels** view, open the context menu on any connected
member and choose **Grant broadcast** / **Revoke broadcast**. The grant is
**session-scoped** — it clears automatically when the client disconnects, so
there is nothing to clean up after a broadcast session ends.

Only **one client may hold the capability at a time.** Attempting to grant it
to a second client while one already holds it is rejected by the server
(`FAILED_PRECONDITION: another client already holds the global-broadcast
capability`).

Grant and revoke are both recorded in the audit log under the `global_broadcast`
action with a `granted` or `revoked` detail.

**What broadcast does on air**

When the broadcaster keys their broadcast PTT:

- The PTT floor is seized on every occupied frequency simultaneously —
  including full-duplex channels.
- Any member currently transmitting on their channel is cut off.
- Local talkers on every frequency are blocked from keying up for the duration
  of the broadcast. On full-duplex channels this includes both PTT and VOX
  (VOX is suppressed for the broadcast's duration).
- Listeners hear only the broadcaster's voice — no other audio passes through.

On release, all floors free again and normal half-duplex or full-duplex rules
resume.

Broadcast **pierces all mutes**: it reaches every listener regardless of
channel-mute or member-mute status. It is an all-hands announcement and is
unmutable by design.

If the broadcaster disconnects mid-broadcast, the server clears the broadcast
and frees all floors automatically.

**Broadcast roger beep + indicator**

Listeners hear a distinct three-step falling tone (C6 → G5 → C5) at the start
of a broadcast. This "broadcast roger" is separate from the normal take-floor
beep and the priority roger, so listeners can immediately tell a server-wide
broadcast from priority traffic on their own channel. The client's center
display also tints **light blue** and reads **BROADCAST** (instead of the
usual green "RECEIVING" or red "CHANNEL BUSY") for the duration, so the cue is
both audible and visible.

### Client: per-user config

The client persists preferences as TOML at the platform's standard config
location:

- macOS: `~/Library/Application Support/toki/config.toml`
- Linux: `~/.config/toki/config.toml`
- Windows: `%APPDATA%\toki\config.toml`

The file is written automatically when settings change. On Unix it's chmodded
to `0600` because it stores the registration password in plaintext. You can
hand-edit it, but most fields are surfaced in the in-app Settings pane.

Schema:

| Section / field | Type | Default | Description |
|---|---|---|---|
| `[connection]` `host` | string | `127.0.0.1` | Server hostname. |
| `[connection]` `port` | u16 | `50051` | Server gRPC port. |
| `[connection]` `display_name` | string | `anon` | Callsign shown to other members. GUI caps to 10 uppercase chars; server caps to 32 bytes. |
| `[connection]` `frequency` | string | `447.00` | Last-selected frequency (`446.00`–`448.00` in `0.05` steps). |
| `[connection]` `password` | string | `""` | Shared secret for password-gated servers. Empty = no password. |
| `[audio]` `input_device` | string? | unset (system default) | cpal input device name. |
| `[audio]` `output_device` | string? | unset (system default) | cpal output device name. |
| `[audio]` `input_gain` | f32 | `1.0` | Linear mic gain. UI range 0.0–2.0; clipped at i16 boundary. |
| `[audio]` `output_gain` | f32 | `1.0` | Linear playback gain. Same range / clipping. |
| `[audio]` `balance` | f32 | `0.0` | Stereo pan for received audio + beeps (−1.0 left … +1.0 right). |
| `[audio]` `noise_suppression` | bool | `true` | Capture-side RNNoise noise filter (strips steady background noise before encoding). Toggleable live in Settings → Voice DSP. |
| `[audio]` `agc` | bool | `true` | Capture-side automatic gain control (eases speech toward a fixed level: −18 dBFS target, up to +18 dB boost / −6 dB cut, speech-gated so pauses don't pump the noise floor). Toggleable live in Settings → Voice DSP. |
| `[audio]` `vox_enabled` | bool | `false` | Voice-activated transmit. When on and tuned to a full-duplex channel, the mic opens automatically on detected speech instead of requiring a held PTT key. Ignored on half-duplex channels. Toggleable live in Settings → Audio → VOICE ACTIV. |
| `[audio]` `vox_sensitivity` | f32 | `0.5` | VOX gate sensitivity, `[0.0, 1.0]`. Higher = more sensitive (opens on quieter speech). Mapped internally to a VAD threshold as `1.0 - sensitivity`, clamped to `[0.15, 0.90]`. Adjustable live in Settings → Audio → VOX SENSITIVITY. |
| `[hotkey]` `binding` | string? | unset | Primary PTT binding, tagged form for any peripheral (e.g. `key:F8`, `mouse:Middle`, `gamepad:0:South`, `streamdeck:0x0fd9:0x0080:3`, `hid:0x046d:0xc52b:2:4`). Preferred over the legacy `key`/`mouse_button` below; written by all new saves. |
| `[hotkey]` `secondary` | string? | unset | Optional fallback PTT binding (same tagged form). PTT fires while *either* the primary or this is held — e.g. a keyboard key backing up a gamepad button. |
| `[hotkey]` `broadcast_ptt` | string? | unset | Optional global-broadcast PTT binding (same tagged form, e.g. `"key:F9"`). Bindable from **Settings → Controls → BROADCAST PTT** as well as here. When held, sends a broadcast PTT the server fans out to every occupied frequency simultaneously. Omitted (unbound) by default. **Inert until an admin grants the global-broadcast capability to this session.** Separate from and additive to the normal PTT — the normal PTT continues to work on the broadcaster's own channel regardless. |
| `[hotkey]` `key` | string? | `"Backquote"` | Legacy PTT keyboard binding (`keyboard_types::Code` variant name). Read-only fallback used only when `binding` is absent. |
| `[hotkey]` `mouse_button` | string? | unset | Legacy PTT mouse binding (`Left`, `Right`, `Middle`, `Mouse4`, …). Read-only fallback used only when `binding` is absent. |
| `[beeps]` `preset` | string | `"default"` | Roger-beep preset id. Unknown ids resolve to `default` at load time. |
| `[beeps]` `volume` | f32 | `0.05` | Beep volume (linear). |
| `[updates]` `auto_check` | bool | `true` | Check GitHub Releases for a newer version on launch + periodically. Notify-only. |

> ⚠️ Note: a legacy single `server = "host:port"` field is migrated to
> `host` + `port` on load. The legacy form is no longer written.

#### Client environment variables

| Variable | Default | Description |
|---|---|---|
| `RUST_LOG` | `info` | tracing filter. Note: on Windows release builds the process has no console attached, so logs are silently dropped. |

### Client identity

On first connect the client mints a persistent, keypair-backed **identity**
and stores it next to the config as `identity.toml` (chmod `0600` — the key
is exactly as sensitive as the stored server password):

- The identity **is** an ed25519 public key. It's displayed everywhere as
  the key's 8-character fingerprint (e.g. `7Q4XF9KB`) — purely derived from
  the key, so renaming yourself never changes your identity string.
- At register, the client signs a server-issued challenge with the private
  key, so an identity string seen in the admin panel or audit log can't be
  claimed by someone who merely saw it.
- A **machine fingerprint** travels alongside: a salted BLAKE3 hash of the
  OS machine id (Linux `/etc/machine-id`, macOS `IOPlatformUUID`, Windows
  `MachineGuid`). The raw identifier never leaves your machine — servers see
  only the hash. Deleting `identity.toml` mints a fresh identity, but the
  machine hash stays the same, so operators can spot config-wipe ban evasion.
- Connecting to an older server (or one without identity support) silently
  falls back to an identity-less session — exactly the pre-0.5 behavior.
  Servers can flip **require identity** in their settings, in which case an
  identity-less register is refused with a clear message.

To **reset** your identity, delete `identity.toml` and reconnect. To **move**
it to another machine, copy the file (the identity follows the key; the
machine hash updates to the new hardware).

## Usage

### Starting the server

```sh
cargo run -p toki-server
# or, after a release build:
./target/release/toki-server
```

Startup log lines you should see:

```
config file loaded (or: no config file resolved … using defaults)
password gate ARMED / DISARMED
data dir resolved
admin store opened (backend = sqlite|mysql|postgres)
TLS ARMED — gRPC channel will serve HTTPS/2
admin panel starting
signaling listening
```

If the admin store is fresh, also expect a single `WARN` line containing the
seeded `admin` username and the auto-generated password. **Copy it out of the
journal** — it is shown once and never recoverable. To reset on SQLite: stop the
server, delete `admin.db`, and restart.

The server shuts down gracefully on `SIGTERM` / `SIGINT` (Ctrl-C), draining in
flight work before exiting — so `docker stop` / systemd / k8s rollouts are clean.

### Starting the client

```sh
cargo run -p toki-client
# or:
./target/release/toki
```

The UI is a single landscape "strip" widget. The main controls:

- **Top bar** — server host:port, display name, settings gear, and an "Update
  available" pill when a newer release is published.
- **Tuner** — left/right chevrons step through the 41-channel `446.00`–`448.00`
  MHz band in `0.05` MHz increments. When the server has named channels enabled,
  the channel's name scrolls as a marquee.
- **Memory presets (M1–M4)** — left-click to save/recall a frequency, left-hold
  to overwrite, right-hold to clear.
- **PTT button** — hold the configured input (default: `` ` ``) or click and hold
  the on-screen button to transmit. A 30-second transmission cap is enforced
  client-side (`TX_LIMIT_MS = 30_000`) — release and re-press to keep talking.
- **Roster / event log** — current members with talking indicators, plus a
  connection/event log.
- **Knobs** — mic gain, speaker gain, and **balance** (pan received audio +
  beeps toward one ear).
- **Voice DSP** — capture-side **noise suppression** (RNNoise) and **auto
  gain** run on your mic signal before it's encoded, so the cleanup benefits
  everyone who hears you. Both are on by default; each has its own toggle in
  Settings → Voice DSP, and turning both off gives a bit-exact raw mic for the
  unprocessed CB character. Toggles apply instantly (next 10 ms frame).
- **Audio self-test** — Settings → Audio shows a live **INPUT LEVEL** bar
  under the input device picker and an **OUTPUT LEVEL** bar under the output
  picker. The input bar moves whenever your mic hears something — *even before
  you connect and without holding PTT* — so you can confirm the right
  microphone is selected and that it's picking you up; it turns amber as the
  signal nears clipping (back off the gain or move away from the mic). The
  **TEST TONE** button plays a short rising chime out the selected output
  device at your current speaker volume — use it to confirm the right
  speaker/earpiece is selected and to check the balance knob (the tone leans
  toward whichever ear you've panned to). The tone needs no connection.
- **Settings** — input/output devices with level meters + test tone, voice DSP
  toggles, PTT binding, roger-beep preset, global hotkeys, and the update-check
  toggle.

#### Binding any input device

PTT, the memory recalls (M1–M4), and the tune up/down hotkeys can each be bound
to **any connected peripheral**: a keyboard key, a mouse button, a game
controller / joystick button, an Elgato Stream Deck key, or a button on an
arbitrary USB HID device. In **Settings**, click **BIND** on a row and *hold the
button you want for ~1 second* — the hold avoids capturing a stray click. The
PTT row also has a **PTT (2ND)** row for an optional **fallback** binding: PTT
fires while either the primary or the secondary is held (e.g. a Stream Deck key
backed up by a keyboard key).

Notes:
- **Keyboard and mouse stay passthrough** — the focused app still receives the
  keystroke. Gamepad / Stream Deck / HID buttons are capture-only (the OS
  doesn't route those to focused apps anyway), and Toki never opens a device
  exclusively, so a game keeps receiving your joystick input.
- **macOS** may require **Input Monitoring** (System Settings → Privacy &
  Security → Input Monitoring) for keyboard/mouse/HID capture. Without it those
  silently see no input; controllers (GameController framework) are unaffected.
- A bound device that's unplugged still shows its label in Settings and simply
  does nothing until reconnected.

### Connecting to a remote server

1. Enter host (DNS name or IP) and port.
2. If the server requires a password, type it in. Empty = open mode.
3. Click **Connect**.

The client uses gRPC over HTTPS/2 with a custom rustls verifier that accepts the
server's self-signed cert (CA-issued certs Just Work too). The application-layer
password + per-packet AEAD provide the actual authentication; TLS is for
transport confidentiality.

The client sends its version on `Register`. The server requires a matching
**MAJOR.MINOR** (patch may differ — patches are wire-compatible) and rejects a
mismatch with `FAILED_PRECONDITION` and a "please update" message, rather than
letting an incompatible build half-connect into silent dead air.

### Duplex modes (half / full)

Full-duplex is gated by a **Settings toggle** (off by default). While off,
every channel is half-duplex, channels can't be set to full, and the duplex
controls + indicators are hidden everywhere. With it on, each channel is
**half-duplex** by default or the operator can switch it to **full-duplex** in
the admin panel (Channels view). The mode is pushed to clients on join /
change-frequency and on live changes.

- **Half-duplex** (classic walkie-talkie): one talker at a time per frequency.
  Pressing PTT while someone else is transmitting puts the radio into a "busy"
  state — your audio is *not* sent, unless you're an elected **priority
  speaker**, whose PTT preempts a non-priority holder.
- **Full-duplex**: no floor. PTT still gates *your* mic (press to talk, only
  hot while held — feedback-safe), but you transmit the instant you press
  (no waiting for a grant), several members can talk at once, and each client
  **mixes** the concurrent streams. Priority speakers don't apply here.
- The 30-second TX cap applies in both modes; the server enforces the same
  invariants regardless of client behavior.

**Roger beeps on full-duplex.** The normal take-floor and clear-floor beeps,
and the priority roger, are suppressed on full-duplex channels — they assume a
single speaker owns the channel, which doesn't hold when everyone can talk at
once. Two cues still play regardless of duplex mode: the broadcast roger
(three-step falling tone at the start of a global broadcast) and the preempted
bump (you were cut off by a broadcast). See the Global Broadcast section for
broadcast behaviour on full-duplex channels.

### Voice activation (VOX)

VOX lets the mic open automatically when you speak, without holding PTT. It is
available on **full-duplex channels only** — on a half-duplex channel the
setting is ignored and PTT works normally.

**Enabling VOX**

In **Settings → Audio**, toggle **VOICE ACTIV.** on. The toggle is off by
default. When on and you are on a full-duplex channel, the mic gate opens
whenever your voice crosses the sensitivity threshold and closes a short time
after you stop speaking.

**Sensitivity and the live indicator**

The **VOX SENSITIVITY** slider (0–100%) sets how eager the gate is: higher =
opens on quieter or softer speech. Internally it maps to a VAD probability
threshold (`1.0 − sensitivity`, clamped between 0.15 and 0.90). While VOX is
enabled, a small dot appears beside the input level meter in Settings and lights
up whenever your voice crosses the current threshold — use it to tune the slider
before you transmit live.

The VAD is the same voice-activity estimate that drives noise suppression, so
steady background noise (fans, HVAC) is filtered out before the gate sees the
signal.

**Hangover**

A ~300 ms hangover (30 × 10 ms frames) prevents the gate from cutting out
between words: the mic stays open until the VAD has been below the threshold
for that many consecutive frames. Brief pauses mid-sentence don't interrupt
your transmission.

**PTT override**

Holding the physical PTT key (or any configured PTT binding) while VOX is on
takes precedence: the key keeps the mic open regardless of the VOX gate state,
and VOX won't close the mic while a PTT key is held.

**Broadcast interaction**

An incoming global broadcast suppresses VOX for its duration — the gate is
forced closed so you cannot accidentally transmit over a fleet-wide
announcement. VOX resumes automatically when the broadcast ends.

### The admin panel

The panel is the **`admin-ui` service**, not the server. Browse to it (e.g.
`https://<ui-host>/`) and log in with the seeded `admin` / password. It speaks
gRPC-Web to the server's `Admin` service and proxies cookie login through the
same origin, so the session cookie (HttpOnly + Secure + SameSite=Strict) stays
same-origin.

Sections:

- **Overview** — live dashboard (gRPC-Web `Watch` stream): members per
  frequency, current PTT holder, session age, and a per-member
  **connection-quality** readout (`RTT · loss`, jitter on hover); updates on
  a 1 Hz tick and immediately after any admin action.
- **Connection quality** — each client measures its own inbound link and
  reports it up: round-trip time (a timestamped probe ridden on the UDP
  keepalive that the server echoes back as a `PONG`), inter-arrival jitter,
  and packet loss (gaps in the server→client sequence). The client shows it
  as 4 signal bars on its radio strip; the admin Rooms column mirrors the
  same verdict. A dash means "not yet measured" (a just-connected member).
- **Metrics & KPIs** — time-series charts of voice-relay bandwidth
  (ingress/egress) and users over time (1h / 24h / 7d), plus uptime / peers /
  transmitting / busiest-channel KPIs and a host-health card (CPU, memory, disk).
  Samples persist at 1-minute resolution (7-day retention).
- **Audit log** — persistent record of admin actions, security events
  (admin + client auth), and peer connect/disconnect. Filter, page, export
  JSONL. Retained 30 days.
- **Channels** — assign human-readable names to frequencies (gated by the
  named-channels toggle), and set each channel's **duplex mode** (Half /
  Full). Both persist independently of room occupancy.
- **Server config** — edit `serverName`, `maxPeers`, `idleKickSecs`,
  `voiceQuality`, the named-channels toggle, the **require-identity** toggle
  (reject clients without a verified identity — makes bans airtight), and
  `grpcPassword` at runtime.
- **Bans** — review and lift identity bans (who, why, by whom, when; a
  "machine" badge marks bans that also cover the machine hash).
- **Account** — rotate the current admin user's password (revokes other
  sessions).

Per-client actions in the roster: **kick**, **move** (to another frequency),
**rename** (broadcasts `DisplayNameChanged`), **priority** (elect/clear a
priority speaker on a channel), **mute**, **ban**, and **grant/revoke broadcast**
(see Global Broadcast below).

**Mute** silences a member's *transmit* without disconnecting them: the
server refuses their PTT presses (`SetMute`), so they stay connected and keep
hearing the channel — the gentle lever between doing nothing and a kick/ban.
Muting whoever currently holds the floor drops it on the spot so the channel
isn't stuck on a now-silent talker; the muted client gets a "muted by an
operator" cue and its PTT button goes red ("UNABLE TO TALK"). Mute is
**session-scoped** (it clears if they reconnect) and audited; the roster shows
a **MUTED** badge.

**Channel mute / No-Talk channels** mute a *whole frequency* (`SetChannelMute`):
while a channel is muted, nobody tuned there may take the floor — **except a
priority speaker**, who keeps their voice. That's the "stage" / "town-hall"
model: a default-muted channel where you grant voice by promoting a member to
**priority speaker** on it. Moving to another (unmuted) channel restores
transmit instantly, since the gate is keyed on the member's current frequency.
An individual **member mute** still outranks a priority grant — a personally
muted speaker stays silent even on a channel where they'd otherwise be the
granted voice. Channel mutes are persisted (across restarts and occupancy, so
you can pre-mute an empty channel) and audited; the panel shows a **MUTED**
badge and a per-channel toggle.

Both mutes run through a single relay-side **speak-gate**
(`member_muted || (channel_muted && !priority)`), so the No-Talk behaviour is
just "default-deny + priority grant" on the same check the per-member mute uses.

**Ban** kicks the session and blocks its *identity* from registering again,
with an optional reason echoed to the banned client and an optional **machine
ban** (a wiped config mints a fresh identity but keeps the machine hash, so it
stays banned). Members without a verified identity can only be kicked — there's
nothing durable to ban.

Members that registered with a verified **client identity** show a
fingerprint badge with their durable identity string (e.g. `7Q4XF9KB`)
next to the display name; hover it for the full public key, the machine-hash
prefix, and when this identity was first seen by the server. The connect line
in the audit log carries the same identity, so a renamed or reconnected
client stays attributable.

### API surface (server)

| Endpoint / RPC | Surface | Notes |
|---|---|---|
| `Signaling.Register` | gRPC | Sends `client_version` + optional signed identity (pubkey, challenge nonce, signature, machine hash); returns `client_id`, `audio_token`, advertised audio endpoint, AEAD key, and the advertised codec. Rejects an incompatible MAJOR.MINOR, a present-but-invalid identity, and a **banned** identity / machine hash (`PERMISSION_DENIED` + the ban reason). Rate-limited per IP. |
| `Signaling.IdentityChallenge` | gRPC | Issues a short-lived (~60 s), stateless nonce the client signs in the subsequent `Register` to prove possession of its identity key. |
| `Signaling.Join` | gRPC server-stream | Pushes `Event`s (members joined/left, PTT, frequency change, rename, channel-name change). |
| `Signaling.Leave` | gRPC | Explicit disconnect. |
| `Signaling.ChangeFrequency` | gRPC | Move between rooms without reopening the event stream. |
| `Signaling.PushToTalk` | gRPC client-stream | Stream PTT key-down/key-up; server fans out to other members. A broadcast PTT (sent when the client holds the broadcast binding and holds the admin-granted global-broadcast capability) seizes floors on all occupied frequencies simultaneously. |
| `Signaling.ReportConnectionQuality` | gRPC | Client pushes its locally-measured RTT / jitter / loss every few seconds; the server denormalizes the latest onto the session for the admin Rooms quality column. Advisory — a dropped report just delays the next refresh. |
| UDP `:50051` | raw UDP | Audio packets: `[16-byte token][1-byte version][8-byte seq][payload][16-byte tag]`, AEAD-sealed (ChaCha20-Poly1305) with the per-session key. Version `0` = keepalive (carries a 16-byte RTT probe); `1` = 10 ms raw-PCM frame (mono i16 LE 48 kHz); `2` = Opus frame whose duration is server-advertised via `RegisterResponse.opus_frame_ms` (10/20/40 ms → 480/960/1920 samples; absent or 0 = legacy 10 ms); `3` = `PONG` (server's RTT-probe echo). Server→peer packets prepend the version. |
| `toki.admin.v1.Admin/*` | gRPC-Web | The admin control plane: `Watch` (server-stream dashboard), operator actions (kick / move / rename / priority / **mute** / **ban** / **grant-broadcast** / **revoke-broadcast**), bans (`BanClient` / `ListBans` / `LiftBan`), runtime config, metrics, health, audit, channel names. Behind the session-cookie auth interceptor. |
| `POST /api/login` | HTTPS | Admin login; sets the session cookie (TTL `session_ttl_hours`). Per-IP rate-limited. |
| `POST /api/logout` | HTTPS | Clears the session cookie. |

## Common Patterns & Recipes

### 1. Open-mode LAN deployment

No config file needed. Run the server, point clients at `lan-host:50051`, done.

```sh
./toki-server
```

### 2. Password-gated public server

```toml
# config.toml
password = "long-shared-secret"

[admin]
bind = "127.0.0.1"   # keep the admin port loopback-only; tunnel via SSH to access
port = 8000
```

Distribute the password out of band. Clients put it in their connection
settings; the server rejects bad passwords with `UNAUTHENTICATED` and applies
exponential backoff per source IP.

### 3. Production server behind a reverse proxy

Run the server with a real cert (or self-signed behind the proxy), and the
admin UI as a second service:

```toml
password = "long-shared-secret"

[tls]
cert = "/etc/letsencrypt/live/toki.example.com/fullchain.pem"
key  = "/etc/letsencrypt/live/toki.example.com/privkey.pem"

[admin]
bind = "0.0.0.0"
port = 8000
```

```sh
export TOKI_DATA_DIR=/var/lib/toki
export TOKI_GRPC_ADDR=0.0.0.0:50051
export TOKI_AUDIO_PUBLIC=toki.example.com:50051   # advertise the public name
./toki-server
```

Then deploy `ellessen/toki-admin-ui` with
`TOKI_SERVER_GRPC_ENDPOINT=https://toki.example.com:8000`, behind your
TLS-terminating proxy (Coolify / Traefik / nginx).

### 4. Docker Compose (server + admin UI + Postgres)

```yaml
services:
  toki-server:
    image: ellessen/toki-server:latest
    restart: unless-stopped
    environment:
      - TOKI_ADMIN_DB_URL=postgres://toki:toki@postgres:5432/toki
      - TOKI_ADMIN_BIND=0.0.0.0
    ports:
      - "50051:50051/tcp"
      - "50051:50051/udp"
      - "8000:8000/tcp"
    volumes:
      - toki-data:/data

  toki-admin-ui:
    image: ellessen/toki-admin-ui:latest
    restart: unless-stopped
    environment:
      - TOKI_SERVER_GRPC_ENDPOINT=https://toki-server:8000
    ports:
      - "8080:80"
    depends_on:
      - toki-server

  postgres:
    image: postgres:15
    restart: unless-stopped
    environment:
      - POSTGRES_USER=toki
      - POSTGRES_PASSWORD=toki
      - POSTGRES_DB=toki
    volumes:
      - toki-db:/var/lib/postgresql/data

volumes:
  toki-data:
  toki-db:
```

The repo's [docker-compose.yml](../docker-compose.yml) ships a source-mounted
dev variant of this stack (`cargo run` + Vite dev server).

### 5. Customizing the PTT binding from the CLI

```toml
[hotkey]
binding       = "key:F8"          # primary trigger
secondary     = "gamepad:0:South" # optional fallback (PTT fires if either is held)
broadcast_ptt = "key:F9"          # global-broadcast PTT (inert until admin grants capability)
```

The `binding` / `secondary` values use the tagged form:

| Form | Example | Meaning |
|------|---------|---------|
| `key:<Code>` | `key:F8` | `keyboard_types::Code` variant name (`Backquote`, `Space`, `F1`–`F24`, `KeyA`, …) |
| `mouse:<label>` | `mouse:Middle` | Mouse button (`Left`, `Right`, `Middle`, `Mouse4`, …) |
| `gamepad:<index>:<button>` | `gamepad:0:South` | Controller button; `index` selects the pad when several are connected (`0` = first) |
| `streamdeck:<vid>:<pid>:<key>` | `streamdeck:0x0fd9:0x0080:3` | Stream Deck key (0-based) |
| `hid:<vid>:<pid>:<byte>:<bit>` | `hid:0x046d:0xc52b:2:4` | Generic HID report bit |

It's easiest to set these from the in-app **Settings** BIND flow rather than by
hand. Restart the client to pick up a hand-edited change. Legacy configs using
`key`/`mouse_button` (without `binding`) still work unchanged.

## Troubleshooting

### "Feedback loop on a single-machine test"

Both clients are sharing the default mic and speakers. **Wear headphones on
every client window**, or run each client on a separate machine.

### The admin panel won't load / browser TLS warning

- The panel is served by the `admin-ui` service — make sure it's running and
  `TOKI_SERVER_GRPC_ENDPOINT` points at the server's admin port.
- For the `Secure` session cookie to be stored, the browser must reach the UI
  over **HTTPS** — terminate TLS at your proxy (Coolify/Traefik) or use the
  Vite dev server (HTTPS on `:5173`).

### Client refuses to connect: "please update" / `FAILED_PRECONDITION`

The client and server disagree on MAJOR.MINOR. The UDP audio wire format can
change across minor versions, so the server rejects mismatches outright. Update
the client (or server) so both are on the same minor.

### "I lost the admin password"

There is intentionally no recovery flow. On SQLite: stop the server, delete
`admin.db`, restart — a fresh `admin` user with a new random password is seeded
and logged at `WARN`. On a remote backend, drop/recreate the `admin_users` row.

### Server logs "password gate DISARMED" but I set `grpcPassword` in the admin panel

The TOML `password` field overrides the runtime `grpcPassword`. If your TOML
sets *any* non-empty password, the admin UI greys the input out. Remove the TOML
field *and* restart, or keep editing the TOML.

### Server won't boot: can't connect to the remote admin DB

The startup retry (~60 s budget) is exhausted — the host/credentials are likely
wrong, or the DB never came up. The error surfaces the (password-redacted)
target. Verify the URL and that the DB accepts TLS connections.

### Clients can connect but no audio reaches the other side

- Confirm UDP `50051` (or your `TOKI_AUDIO_ADDR`) is reachable from the client.
  Firewalls + NAT often allow the TCP gRPC handshake but silently drop UDP.
- When the server is behind NAT, set `TOKI_AUDIO_PUBLIC` to the externally
  reachable host. Note the client connects UDP on the **same port it reached
  gRPC on**, ignoring any different port in `TOKI_AUDIO_PUBLIC` — so make sure
  audio is exposed on the gRPC port. If you moved gRPC to a non-default port
  (e.g. `:50052`), audio must be reachable there too.
- The server learns the client's UDP source from the version-`0` keepalive. If
  clients never send one (broken outbound UDP path), the server has nowhere to
  relay audio to.

### `RESOURCE_EXHAUSTED` on registration

You hit `maxPeers` (default 256). Bump it in the admin panel's Server Config
section.

### Broadcast PTT is bound but does nothing

The `broadcast_ptt` binding is inert until an admin grants the global-broadcast
capability to your session from the Channels view (member context menu →
**Grant broadcast**). The binding has no effect in open mode or before a grant —
this is by design.

### Admin panel rejects "Grant broadcast" with `FAILED_PRECONDITION`

Another connected client already holds the global-broadcast capability. Only one
client may hold it at a time. Revoke it from the current holder first (member
context menu → **Revoke broadcast**), then grant it to the new client.

### VOX doesn't open my mic

Check each of these in order:

1. **Full-duplex channel required.** VOX is ignored on half-duplex channels. Confirm the channel badge shows **FULL** (the admin must have enabled full-duplex in the Channels view, and the server's full-duplex toggle must be on in Server Config).
2. **VOICE ACTIV. must be on.** Open Settings → Audio and confirm the toggle is enabled.
3. **Sensitivity may be too low.** The live dot beside the input level meter in Settings lights when your voice would cross the current threshold. If it doesn't light when you speak, raise the VOX SENSITIVITY slider.
4. **A broadcast is in flight.** VOX is suppressed for the duration of a global broadcast; it resumes automatically when the broadcast ends.

### I hear no beeps when someone keys up on this channel

On a full-duplex channel the normal take-floor, clear-floor, and priority roger
beeps are suppressed — they assume a single floor owner, which doesn't apply when
multiple members can transmit simultaneously. Two cues still play on full-duplex:
the broadcast roger (three-step falling tone) and the preempted bump (you were
cut off by a broadcast). If you're on a half-duplex channel and still hear no
beeps, check Settings → Audio → BEEP VOLUME.

### `cargo build` fails with "could not find `protoc`"

Install the protobuf compiler (see Prerequisites) and ensure it's on `PATH`.
The `toki-proto` crate runs `protoc` via `tonic-build` at compile time.

### Linux client fails to start with a missing `libasound`/`libGL`/`libxkbcommon`/`libopus` error

Install the system deps listed in Prerequisites. The CI workflow at
[.github/workflows/ci.yml](../.github/workflows/ci.yml) keeps an authoritative
list.

### Windows release client appears silent in logs

The Windows release build uses `windows_subsystem = "windows"`, which detaches
from the console. `tracing` writes are silently dropped. Use a debug build
(`cargo run -p toki-client`) for log output.

### Cross-compiling a Windows `.exe` from macOS

The shipped Windows binary is built by CI on a native `windows-latest` MSVC
runner (`.github/workflows/ci.yml`). For **local testing** you can cross-compile
a `windows-gnu` (mingw) `.exe` from macOS, but `audiopus_sys` (Opus) can't build
its C library for Windows on a Mac host, so you must supply a prebuilt mingw
libopus:

```sh
brew install mingw-w64 automake autoconf libtool pkg-config
rustup target add x86_64-pc-windows-gnu

scripts/build-opus-mingw.sh        # builds vendor/opus-mingw/.../libopus.a (once)
scripts/build-windows-cross.sh --release
# → target/x86_64-pc-windows-gnu/release/toki.exe
```

The wrapper sets `OPUS_LIB_DIR` / `OPUS_STATIC` / `OPUS_NO_PKG` only for the
cross-build (they'd break a native macOS build) and clears the `audiopus_sys`
build cache so cargo re-resolves the link path. This produces a GNU-ABI binary
for testing — not the MSVC binary that's actually released.

## Architecture Overview

The repo is a Cargo workspace (three crates) plus a standalone web UI:

```
crates/
  proto/    # toki.proto + tonic-generated types + UDP wire format
  server/   # gRPC signaling + UDP audio relay + admin gRPC-Web service
  client/   # egui desktop client
admin-ui/   # React/Vite admin SPA (its own Docker image)
```

Key server modules ([crates/server/src/](../crates/server/src/)):

- `signaling.rs` — the gRPC `Signaling` service (Register / Join / Leave /
  ChangeFrequency / PushToTalk) and the version-compatibility gate.
- `audio.rs` — UDP listener; verifies the per-session AEAD tag + sequence,
  pins the session to its registering IP, and re-seals opaque payloads to every
  other member (the server never decodes audio).
- `state.rs` — `Registry`: clients, rooms, PTT floor, priority. The single
  mutable structure all subsystems share via `Arc<Mutex<…>>`.
- `reaper.rs` — evicts clients whose `last_seen` exceeds `idleKickSecs`.
- `throttle.rs` — per-IP rate cap on `Register` and exponential auth-failure
  backoff.
- `config.rs` / `server_config.rs` — boot-time TOML vs runtime-mutable settings.
- `tls.rs` — loads operator certs or auto-generates a self-signed pair (`rcgen`).
- `metrics.rs` — bandwidth/users sampling + host health (`sysinfo`).
- `audit.rs` — async audit-log writer + producers.
- `admin/` — the admin control plane: `grpc.rs` (gRPC-Web `Admin` service +
  `Watch`), `db.rs` (SQLx multi-backend store), `auth.rs` (argon2id + BLAKE3
  session cookies), `watch.rs` (snapshot broadcaster), `handlers.rs` (the
  `/api/login` + `/api/logout` cookie endpoints), `routes.rs`, `mod.rs`.
- `validation.rs` — server-side bounds on display names, frequencies, UUIDs.
- `bin/main.rs` — wiring + graceful SIGTERM/SIGINT shutdown.

Key client modules ([crates/client/src/](../crates/client/src/)):

- `app.rs` — egui application: paints the strip widget, snapshots runtime state.
- `runtime.rs` — owns the gRPC connection, event stream, and audio plumbing
  (incl. Opus encode/decode and PTT-release flush).
- `audio.rs` — cpal input/output on a dedicated thread; PTT-gated capture;
  inbound feeds a latency-managed playback ring.
- `hotkey.rs` — global input binding across keyboard/mouse (device_query),
  gamepads (gilrs), and Stream Deck / HID (hidapi); passive, non-exclusive.
- `update.rs` — GitHub Releases update check (notify-only).
- `config.rs` — persisted user preferences.
- `state.rs` — connection + UI state shared between runtime and GUI.
- `theme.rs` — design tokens (colors, sizes, the `TX_LIMIT_MS = 30_000` cap).

Voice is **Opus** by default (~24 kbps/stream — a ~15–20× cut vs raw PCM once
per-packet overhead is counted), with 10 ms frames matching the capture cadence
so there's no encoder buffering. Operators can drop to raw PCM or dial Opus
quality from the admin panel; the codec lives entirely on the clients (the
server relays opaque encrypted payloads), so adding future codecs is a
client-only change plus a wire-version byte.

**Opus frame size.** The server advertises a frame duration (10/20/40 ms) in
`RegisterResponse.opus_frame_ms`; the client encoder accumulates that many
10 ms capture chunks before sending one UDP packet. At 24 kbps/10 ms the
payload is only ~30 B while per-packet overhead is ~53 B, so doubling the
frame to 20 ms halves packet rate and cuts relay egress materially. Stacks with
DTX (silence suppression) for further savings during speech pauses.

The latency cost is +10 ms at 20 ms frames, +30 ms at 40 ms frames
(mouth-to-ear). The default (10 ms) preserves the original low-latency
behaviour.

**Client-compatibility warning.** Clients older than the release that added
`opus_frame_ms` carry a decode buffer sized for ≤30 ms (1440 samples). A 40 ms
frame (1920 samples) would have overflowed that buffer — the bug is fixed in
the same release. Do not raise `opusFrameMs` above `10` until every connected
client is on this release or newer.
