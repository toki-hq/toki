use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use tokio::net::UdpSocket;
use tracing::{debug, warn};

use chacha20poly1305::{
    aead::{generic_array::GenericArray, AeadInPlace},
    ChaCha20Poly1305, Key, KeyInit, Nonce, Tag,
};
use toki_proto::wire::{
    build_nonce, FRAME_BYTES, HEADER_LEN_C2S, HEADER_LEN_S2C, MAX_AUDIO_PACKET, SEQ_LEN, TAG_LEN,
    TOKEN_LEN, VERSION_AUDIO_PCM,
};

use crate::state::{hash_token, SharedRegistry};

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

pub async fn run(
    bind: SocketAddr,
    registry: SharedRegistry,
    counters: crate::metrics::SharedByteCounters,
) -> anyhow::Result<()> {
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
        // Ingress accounting: count every received datagram (incl.
        // keepalives + rejected packets) — it's real bytes on the wire.
        counters.add_rx(len as u64);

        if len < HEADER_LEN_C2S {
            debug!(len, "packet too small");
            continue;
        }

        // Inbound (C2S) header layout (see toki_proto::wire docs):
        //   [0..16]    token
        //   [16]       version (AEAD associated data)
        //   [17..25]   seq (le u64) — also drives the AEAD nonce
        //   [25..41]   Poly1305 tag (16 bytes)
        //   [41..]     ChaCha20 ciphertext of the payload
        let token = &buf[..TOKEN_LEN];
        let version = buf[TOKEN_LEN];
        let seq_bytes: [u8; SEQ_LEN] = buf[TOKEN_LEN + 1..TOKEN_LEN + 1 + SEQ_LEN]
            .try_into()
            .expect("slice has SEQ_LEN bytes");
        let seq = u64::from_le_bytes(seq_bytes);
        let tag_bytes: [u8; TAG_LEN] = buf[TOKEN_LEN + 1 + SEQ_LEN..HEADER_LEN_C2S]
            .try_into()
            .expect("slice has TAG_LEN bytes");
        let ciphertext_in = &buf[HEADER_LEN_C2S..len];

        // Strict shape check for audio frames: legitimate audio packets
        // are *exactly* HEADER_LEN_C2S + FRAME_BYTES long (ChaCha20 is
        // a stream cipher, so ciphertext length == plaintext length).
        // Keepalive packets have zero-length ciphertext. Any other
        // shape is malformed or hostile.
        if version == VERSION_AUDIO_PCM && ciphertext_in.len() != FRAME_BYTES {
            debug!(
                len,
                expected = HEADER_LEN_C2S + FRAME_BYTES,
                "audio frame wrong size, dropping"
            );
            continue;
        }
        if version != VERSION_AUDIO_PCM && !ciphertext_in.is_empty() {
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

        // Hold the lock once: authenticate, decrypt, replay-check,
        // and (for audio frames) snapshot per-peer keys + outbound
        // seqs for the fan-out. We release before doing any send_to
        // calls so the network path can't backpressure into the lock.
        let dispatch: Option<(Vec<u8>, Vec<PeerTarget>)> = {
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
            let (session_key, last_seq) = if let Some(client) = registry.clients.get(&sender_id) {
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

            // AEAD decrypt: ChaCha20-Poly1305 with AAD = [version] so
            // an attacker can't repurpose a tag computed for one
            // version onto a packet of another. The seq becomes the
            // nonce (zero-padded). decrypt_in_place_detached returns
            // Err on any of: wrong key, modified ciphertext, wrong
            // AAD, wrong tag.
            let cipher = ChaCha20Poly1305::new(Key::from_slice(&session_key));
            let nonce_bytes = build_nonce(seq);
            let nonce = Nonce::from_slice(&nonce_bytes);
            let tag = Tag::from_slice(&tag_bytes);
            let mut plaintext = ciphertext_in.to_vec();
            if cipher
                .decrypt_in_place_detached(nonce, &[version], &mut plaintext, tag)
                .is_err()
            {
                warn!(?peer, client = %sender_id, "audio packet AEAD verify failed, dropping");
                continue;
            }

            // Strict-monotonic replay protection. The first valid
            // packet on a session always has seq > 0 (client starts
            // at 1), which beats the initial audio_last_seq = 0.
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
                client.last_seen = std::time::Instant::now();
            }

            if version != VERSION_AUDIO_PCM {
                // Keepalive: address + seq + heartbeat updated above;
                // nothing to forward.
                continue;
            }

            // Walkie-talkie fan-out. Only forward audio from the
            // sender's current-frequency-room PTT holder. We collect
            // (addr, key, seq) triples while still under the lock,
            // bumping each peer's outbound seq as we go so two
            // back-to-back senders can't share a nonce.
            let frequency = registry
                .clients
                .get(&sender_id)
                .and_then(|c| c.current_frequency.clone());
            let Some(freq) = frequency else { continue };

            let member_ids: Vec<String> = {
                let Some(room) = registry.rooms.get(&freq) else {
                    continue;
                };
                if room.holder.as_deref() != Some(sender_id.as_str()) {
                    continue;
                }
                room.members
                    .iter()
                    .filter(|id| *id != &sender_id)
                    .cloned()
                    .collect()
            };

            let mut targets: Vec<PeerTarget> = Vec::with_capacity(member_ids.len());
            for id in member_ids {
                if let Some(other) = registry.clients.get_mut(&id) {
                    if let Some(addr) = other.audio_addr {
                        let seq = other.audio_outbound_seq;
                        other.audio_outbound_seq = seq.saturating_add(1);
                        targets.push(PeerTarget {
                            addr,
                            key: other.audio_mac_key,
                            seq,
                        });
                    }
                }
            }
            Some((plaintext, targets))
        };

        let Some((plaintext, targets)) = dispatch else {
            continue;
        };

        // Re-encrypt for each peer with their own session key and
        // outbound seq, then send. The send loop runs *outside* the
        // registry lock so a slow network path can't stall the
        // entire relay.
        for target in targets {
            let cipher = ChaCha20Poly1305::new(GenericArray::from_slice(&target.key));
            let nonce_bytes = build_nonce(target.seq);
            let nonce = Nonce::from_slice(&nonce_bytes);
            let mut buf_ct = plaintext.clone();
            let tag =
                match cipher.encrypt_in_place_detached(nonce, &[VERSION_AUDIO_PCM], &mut buf_ct) {
                    Ok(t) => t,
                    Err(e) => {
                        warn!(error = %e, "AEAD encrypt failed for outbound peer");
                        continue;
                    }
                };
            // S2C layout: seq (8) | tag (16) | ciphertext
            let mut pkt = Vec::with_capacity(HEADER_LEN_S2C + buf_ct.len());
            pkt.extend_from_slice(&target.seq.to_le_bytes());
            pkt.extend_from_slice(tag.as_slice());
            pkt.extend_from_slice(&buf_ct);
            match socket.send_to(&pkt, target.addr).await {
                Ok(sent) => counters.add_tx(sent as u64),
                Err(e) => warn!(error = %e, "failed to forward audio"),
            }
        }
    }
}

/// Snapshot of what we need to send to each peer in the fan-out.
/// Collected under the registry lock; the actual `send_to` happens
/// outside so a slow recipient can't block the relay.
struct PeerTarget {
    addr: SocketAddr,
    key: [u8; toki_proto::wire::MAC_KEY_LEN],
    seq: u64,
}
