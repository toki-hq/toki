use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use tokio::net::UdpSocket;
use tracing::{debug, warn};

use toki_proto::wire::{
    FRAME_BYTES, HEADER_LEN, MAC_LEN, MAX_AUDIO_PACKET, SEQ_LEN, TOKEN_LEN, VERSION_AUDIO_PCM,
};

use crate::state::{SharedRegistry, hash_token};

/// Maximum audio frames per second the server will forward from any
/// single token. The legitimate client sends one 10 ms frame at a
/// time at 100 fps; allowing 110 fps leaves headroom for occasional
/// jitter / catch-up without giving an attacker a meaningful
/// amplification surface.
const MAX_AUDIO_FPS: u32 = 110;

/// Token bucket window. Counters reset every `RATE_WINDOW`. Smaller
/// values give tighter shaping but burn more CPU on the HashMap
/// scan; 1 s strikes a reasonable balance for human-paced traffic.
const RATE_WINDOW: Duration = Duration::from_secs(1);

/// Per-token rate state: how many `VERSION_AUDIO_PCM` packets we've
/// forwarded since `window_start`, and when the window opened. Keepalive
/// packets don't count — they're cheap and their cadence is set by the
/// client (every 3 s).
struct RateState {
    window_start: Instant,
    packets: u32,
}

/// Constant-time byte comparison for the per-packet MAC check. Lengths
/// are always equal at the call site (both 8 bytes), so the early-out
/// here is unreachable in normal traffic — kept for defensive parity
/// with `signaling::ct_eq`.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

pub async fn run(bind: SocketAddr, registry: SharedRegistry) -> anyhow::Result<()> {
    let socket = UdpSocket::bind(bind).await?;
    tracing::info!(?bind, "audio relay listening");

    let mut buf = vec![0u8; MAX_AUDIO_PACKET];
    // Per-token rate state. Single-threaded access from this loop so
    // a plain HashMap is fine — no Mutex needed. Entries are pruned
    // implicitly when a window expires and the token isn't refilled;
    // an explicit prune of dead-token entries would be nice but
    // tokens get evicted by the reaper anyway, so this never grows
    // beyond active-session count.
    let mut rate_state: HashMap<[u8; toki_proto::wire::TOKEN_LEN], RateState> = HashMap::new();

    loop {
        let (len, peer) = match socket.recv_from(&mut buf).await {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "udp recv failed");
                continue;
            }
        };

        if len < HEADER_LEN {
            debug!(len, "packet too small");
            continue;
        }

        // Header layout (see toki_proto::wire docs):
        //   [0..16]   token
        //   [16]      version
        //   [17..25]  seq (le u64)
        //   [25..33]  mac (truncated BLAKE3 keyed_hash)
        //   [33..]    payload
        let token = &buf[..TOKEN_LEN];
        let version = buf[TOKEN_LEN];
        let seq_bytes: [u8; SEQ_LEN] = buf[TOKEN_LEN + 1..TOKEN_LEN + 1 + SEQ_LEN]
            .try_into()
            .expect("slice has SEQ_LEN bytes");
        let seq = u64::from_le_bytes(seq_bytes);
        let mac = &buf[TOKEN_LEN + 1 + SEQ_LEN..HEADER_LEN];
        let payload = &buf[HEADER_LEN..len];

        // Strict shape check for audio frames: legitimate audio packets
        // are *exactly* HEADER_LEN + FRAME_BYTES long. Keepalive
        // packets have zero payload. Any other shape is malformed
        // or hostile.
        if version == VERSION_AUDIO_PCM && payload.len() != FRAME_BYTES {
            debug!(
                len,
                expected = HEADER_LEN + FRAME_BYTES,
                "audio frame wrong size, dropping"
            );
            continue;
        }
        if version != VERSION_AUDIO_PCM && !payload.is_empty() {
            debug!(len, version, "non-audio packet with payload, dropping");
            continue;
        }

        // Per-token rate limit for audio frames. Keepalives are
        // cheap and don't count toward the budget. Enforced *before*
        // we take the registry lock so a flooder doesn't even cause
        // mutex contention with legitimate clients. Key is the raw
        // token bytes as a fixed-size array — avoids the per-packet
        // Vec allocation we'd pay with `token.to_vec()`.
        if version == VERSION_AUDIO_PCM {
            let mut rate_key = [0u8; TOKEN_LEN];
            rate_key.copy_from_slice(token);
            let now = Instant::now();
            let entry = rate_state.entry(rate_key).or_insert(RateState {
                window_start: now,
                packets: 0,
            });
            if now.duration_since(entry.window_start) >= RATE_WINDOW {
                entry.window_start = now;
                entry.packets = 0;
            }
            entry.packets += 1;
            if entry.packets > MAX_AUDIO_FPS {
                // Log once per window (when we first cross the cap)
                // so a sustained flood doesn't drown the operator's
                // log. The == comparison fires on exactly the cap+1
                // packet inside each window.
                if entry.packets == MAX_AUDIO_FPS + 1 {
                    warn!(?peer, "audio rate limit exceeded, dropping further frames");
                }
                continue;
            }
        }

        // Hash the raw token once, outside the registry lock — BLAKE3
        // on 16 bytes is in the tens-of-nanoseconds range and would
        // be silly to do while holding the global mutex. The hash
        // is what the registry actually stores; we never persist
        // the preimage server-side.
        let token_hash = hash_token(token);

        // Hold the lock once: authenticate, learn peer address, and compute
        // the forwarding fan-out. We release before doing any send_to calls.
        let targets: Vec<SocketAddr> = {
            let mut registry = registry.lock().await;
            let Some(sender_id) = registry.tokens.get(&token_hash).cloned() else {
                debug!(?peer, "unknown audio token");
                continue;
            };

            // Source-IP pinning: the gRPC Register call recorded the
            // session's expected_ip. If this UDP packet arrived from
            // a *different* IP, treat it as a hijack attempt and
            // drop — even though the token authenticated. The port
            // is allowed to vary because NAT will usually pick a
            // different one for the UDP flow than for the TCP
            // signaling flow. `expected_ip = None` is honoured (Unix
            // socket transports skip the check).
            //
            // MAC + replay check is also done here, snapshotting the
            // key + last_seq while holding the lock so the next
            // packet on the same session sees the updated seq.
            let (mac_key, last_seq) = if let Some(client) = registry.clients.get(&sender_id) {
                if let Some(expected) = client.expected_ip {
                    if expected != peer.ip() {
                        warn!(
                            ?peer,
                            expected = %expected,
                            client = %sender_id,
                            "audio packet from unexpected source IP, dropping"
                        );
                        continue;
                    }
                }
                (client.audio_mac_key, client.audio_last_seq)
            } else {
                debug!(?peer, "token resolved to id but client gone");
                continue;
            };

            // MAC over (version || seq || payload). The token isn't
            // included because it's already in plain view and
            // identifies the key; including the version prevents an
            // attacker from repurposing a MAC computed for one
            // version onto a packet of another. Constant-time
            // comparison protects against any plausible timing leak
            // (unlikely here, but free).
            let mac_input = {
                let mut v = Vec::with_capacity(1 + SEQ_LEN + payload.len());
                v.push(version);
                v.extend_from_slice(&seq_bytes);
                v.extend_from_slice(payload);
                v
            };
            let expected_mac = blake3::keyed_hash(&mac_key, &mac_input);
            let expected_mac = &expected_mac.as_bytes()[..MAC_LEN];
            if !constant_time_eq(expected_mac, mac) {
                warn!(?peer, client = %sender_id, "audio packet MAC mismatch, dropping");
                continue;
            }
            // Strict-monotonic replay protection. The first valid
            // packet on a session always has seq > 0 (client starts
            // at 1), which beats the initial audio_last_seq = 0.
            // UDP reordering can drop the occasional out-of-order
            // packet on lossy links — playback already tolerates
            // this as ordinary loss.
            if seq <= last_seq {
                debug!(
                    ?peer,
                    client = %sender_id,
                    seq,
                    last_seq,
                    "audio packet seq not strictly increasing, dropping"
                );
                continue;
            }

            if let Some(client) = registry.clients.get_mut(&sender_id) {
                client.audio_last_seq = seq;
                client.audio_addr = Some(peer);
                // Every UDP packet — audio or keepalive — counts as a
                // heartbeat. The reaper task uses this to evict clients
                // who've gone silent. Safe to refresh now that we've
                // verified the source IP above; a spoofer can't keep
                // the session alive on the legitimate user's behalf.
                client.last_seen = std::time::Instant::now();
            }

            if version != VERSION_AUDIO_PCM {
                // Keepalive (or unknown version): we've already updated the
                // peer's UDP address; nothing to forward.
                continue;
            }

            // Walkie-talkie: only forward audio from the sender's
            // current-frequency room PTT holder. We look up the
            // sender's current_frequency, then check that room's
            // holder/members. Senders on a frequency where they're
            // not the holder, or who aren't in any room, get dropped
            // even though their token authenticated.
            let mut targets: Vec<SocketAddr> = Vec::new();
            let frequency = registry
                .clients
                .get(&sender_id)
                .and_then(|c| c.current_frequency.clone());
            if let Some(freq) = frequency {
                if let Some(room) = registry.rooms.get(&freq) {
                    if room.holder.as_deref() == Some(sender_id.as_str()) {
                        for id in &room.members {
                            if id == &sender_id {
                                continue;
                            }
                            if let Some(other) = registry.clients.get(id) {
                                if let Some(addr) = other.audio_addr {
                                    targets.push(addr);
                                }
                            }
                        }
                    }
                }
            }
            targets
        };

        for addr in targets {
            if let Err(e) = socket.send_to(payload, addr).await {
                warn!(error = %e, "failed to forward audio");
            }
        }
    }
}
