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
    build_nonce, decode_ping, is_audio, FRAME_BYTES, HEADER_LEN_C2S, HEADER_LEN_S2C,
    MAX_AUDIO_PACKET, MAX_OPUS_PAYLOAD, PING_LEN, SEQ_LEN, TAG_LEN, TOKEN_LEN, VERSION_AUDIO_OPUS,
    VERSION_AUDIO_PCM, VERSION_KEEPALIVE, VERSION_PONG,
};

use crate::state::{hash_token, SharedRegistry};

/// Maximum audio frames per second the server will forward from any
/// single token. The legitimate client sends one 10 ms frame at a
/// time at 100 fps; allowing 110 fps leaves headroom for occasional
/// jitter / catch-up without giving an attacker a meaningful
/// amplification surface.
const MAX_AUDIO_FPS: u32 = 110;

/// How long after a PTT release the relay keeps forwarding the
/// just-released holder's audio. The PTT-release rides the reliable gRPC
/// stream and clears the room's holder before the talker's final UDP
/// voice frames land; this window lets those tail frames (which can lag
/// the release by up to ~1 RTT of network + jitter) through so the end
/// of speech isn't clipped. Only the immediately-previous holder
/// benefits, and only while the floor is free — a new presser voids it.
const RELEASE_GRACE: Duration = Duration::from_millis(200);

/// Walkie-talkie relay gate: may we forward an audio packet from
/// `sender`? `last_released` is the just-released holder and how long
/// ago they let go (`None` if no one has released since the last grant).
///
/// True when the sender holds the floor now, or when the floor is free
/// and the sender is the most recent holder still inside `grace` — that
/// second arm carries the talker's final UDP frames, which routinely lag
/// the reliable PttUp that already cleared the holder. A new holder (a
/// fresh press or a priority preemption) sets `holder` to `Some(_)` and
/// clears `last_released`, so a previous holder's tail can never bleed
/// into someone else's transmission.
fn should_relay(
    holder: Option<&str>,
    last_released: Option<(&str, Duration)>,
    sender: &str,
    grace: Duration,
) -> bool {
    if holder == Some(sender) {
        return true;
    }
    holder.is_none() && matches!(last_released, Some((id, since)) if id == sender && since < grace)
}

/// Is a decrypted packet's payload the right length for its `version`?
/// ChaCha20 is a stream cipher, so ciphertext length equals plaintext
/// length — this validates the *shape* before we act on the packet.
///
///   * PCM — exactly one [`FRAME_BYTES`] frame.
///   * Opus — non-empty, up to [`MAX_OPUS_PAYLOAD`].
///   * Keepalive — empty (legacy) **or** exactly a [`PING_LEN`] RTT probe
///     (since 0.5.0). The probe case is the one that regressed: a guard
///     that demanded keepalives be empty silently dropped every probe-
///     carrying keepalive, so `last_seen` never refreshed (reaper evicted
///     the client) and no pong was ever sent (client RTT never measured).
///   * Anything else (unknown version with a payload) — rejected.
fn payload_shape_ok(version: u8, payload_len: usize) -> bool {
    match version {
        VERSION_AUDIO_PCM => payload_len == FRAME_BYTES,
        VERSION_AUDIO_OPUS => payload_len != 0 && payload_len <= MAX_OPUS_PAYLOAD,
        VERSION_KEEPALIVE => payload_len == 0 || payload_len == PING_LEN,
        // PONG is server→client only; a client should never send one, and
        // any other version is unknown. Reject a payload either way (an
        // empty unknown-version packet is harmless and still dropped later
        // by the codec routing).
        _ => payload_len == 0,
    }
}

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

/// Drop rate-limit entries whose window has gone stale — i.e. tokens that
/// haven't sent an audio frame within the last `RATE_WINDOW`. A live
/// transmitter refills its entry every window, so this only ever removes
/// idle, disconnected, or spoofed-token entries; a token that transmits
/// again just re-inserts with a fresh window (and a fresh, empty budget,
/// which is correct — it wasn't mid-window). Keeps the map bounded to
/// recently-active tokens instead of every token ever seen.
fn prune_rate_state(
    rate_state: &mut HashMap<[u8; toki_proto::wire::TOKEN_LEN], RateState>,
    now: Instant,
) {
    rate_state.retain(|_, s| now.duration_since(s.window_start) < RATE_WINDOW);
}

pub async fn run(
    bind: SocketAddr,
    registry: SharedRegistry,
    counters: crate::metrics::SharedByteCounters,
) -> anyhow::Result<()> {
    let socket = UdpSocket::bind(bind).await?;
    tracing::info!(?bind, "audio relay listening");

    let mut buf = vec![0u8; MAX_AUDIO_PACKET];
    // Per-token rate state. Single-threaded access from this loop so a
    // plain HashMap is fine — no Mutex needed. The key is the raw token
    // *off the wire*, inserted before the registry lookup (so a flooder
    // can't make us take the registry lock — see the rate check below).
    // That means a spoofed or stale token also creates an entry, and a
    // window expiring only *resets* an entry, never removes it — so
    // without an explicit sweep the map would grow unbounded under a
    // distinct-token flood. `prune_rate_state` drops entries whose window
    // has gone stale, bounding the map to recently-active tokens.
    let mut rate_state: HashMap<[u8; toki_proto::wire::TOKEN_LEN], RateState> = HashMap::new();
    // Sweep `rate_state` at most once per window. Bounds both the map
    // size and the amortized prune cost (one full scan / sec, not per
    // packet) regardless of inbound rate.
    let mut last_prune = Instant::now();

    loop {
        let (len, peer) = match socket.recv_from(&mut buf).await {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "udp recv failed");
                continue;
            }
        };
        // Periodic cleanup of dead-token rate entries. Cheap (a single
        // retain scan), gated to once per `RATE_WINDOW` so it stays O(1)
        // amortized per packet even under a flood.
        let now = Instant::now();
        if now.duration_since(last_prune) >= RATE_WINDOW {
            prune_rate_state(&mut rate_state, now);
            last_prune = now;
        }
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

        // Per-codec payload-shape check (see `payload_shape_ok`). ChaCha20
        // is a stream cipher, so ciphertext length equals plaintext length.
        if !payload_shape_ok(version, ciphertext_in.len()) {
            debug!(len, version, "packet payload wrong shape, dropping");
            continue;
        }

        // Per-token rate limit for audio frames. Keepalives are
        // cheap and don't count toward the budget. Enforced *before*
        // we take the registry lock so a flooder doesn't even cause
        // mutex contention with legitimate clients. Key is the raw
        // token bytes as a fixed-size array — avoids the per-packet
        // Vec allocation we'd pay with `token.to_vec()`.
        if is_audio(version) {
            let mut rate_key = [0u8; TOKEN_LEN];
            rate_key.copy_from_slice(token);
            // Reuse the loop-level `now` snapshot taken above.
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
        let dispatch: Option<(Vec<u8>, u32, Vec<PeerTarget>)> = {
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

            if !is_audio(version) {
                // Keepalive: address + seq + heartbeat updated above;
                // nothing to forward to peers. But if it carries an RTT
                // probe, bounce it straight back as a PONG so the client
                // can measure round-trip time. We seal the echo with the
                // sender's own session key + a fresh outbound seq (same
                // S2C framing as audio), so it's authenticated and
                // replay-protected exactly like a voice packet.
                if version == VERSION_KEEPALIVE {
                    if let Some((ping_id, send_ts)) = decode_ping(&plaintext) {
                        let pong = {
                            let Some(client) = registry.clients.get_mut(&sender_id) else {
                                continue;
                            };
                            let out_seq = client.audio_outbound_seq;
                            client.audio_outbound_seq = out_seq.saturating_add(1);
                            seal_pong(&session_key, out_seq, ping_id, send_ts)
                        };
                        if let Some(pkt) = pong {
                            if let Err(e) = socket.send_to(&pkt, peer).await {
                                warn!(error = %e, ?peer, "failed to send pong");
                            }
                        }
                    }
                }
                continue;
            }

            // Speak-gate backstop. The signaling PTT path already
            // refuses a muted member's press (and the SetMute handler
            // drops the floor the instant a mute lands), so a muted
            // sender normally fails `should_relay` below anyway. This
            // direct check closes the narrow race where the member's
            // final in-flight UDP frames arrive within the release-grace
            // window after the floor was dropped out from under them —
            // we don't want a just-muted talker's tail leaking out.
            let sender_speaks = registry
                .clients
                .get(&sender_id)
                .map(|c| c.can_speak())
                .unwrap_or(false);
            if !sender_speaks {
                continue;
            }

            // Collect the sender's audio_id for S2C header demux (full-duplex
            // concurrent talkers need a routing id; harmless in half-duplex).
            let sender_audio_id = registry
                .clients
                .get(&sender_id)
                .map(|c| c.audio_id)
                .unwrap_or(0);

            // ── Global-broadcast fan-out ──────────────────────────────────
            // When this sender is the active broadcaster, deliver to every
            // connected client (except themselves). The normal per-room relay
            // path is skipped entirely: broadcast pierces all rooms and mutes.
            let is_broadcaster = registry.broadcast_active.as_deref() == Some(sender_id.as_str());
            if is_broadcaster {
                let mut targets: Vec<PeerTarget> = Vec::with_capacity(registry.clients.len());
                // Collect in client insertion order (deterministic enough for
                // relay; no ordering guarantee needed). Excludes the sender.
                let peer_ids: Vec<String> = registry
                    .clients
                    .keys()
                    .filter(|id| *id != &sender_id)
                    .cloned()
                    .collect();
                for id in peer_ids {
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
                Some((plaintext, sender_audio_id, targets))
            } else {
                // ── Normal per-room fan-out ───────────────────────────────
                // Walkie-talkie fan-out. We collect (addr, key, seq) triples
                // while still under the lock, bumping each peer's outbound seq
                // as we go so two back-to-back senders can't share a nonce.
                let frequency = registry
                    .clients
                    .get(&sender_id)
                    .and_then(|c| c.current_frequency.clone());
                let Some(freq) = frequency else { continue };

                let member_ids: Vec<String> = {
                    let Some(room) = registry.rooms.get(&freq) else {
                        continue;
                    };
                    // Full-duplex: no floor — forward every member's audio (the
                    // client self-gates by only sending while PTT is held).
                    // Half-duplex: forward only the current floor holder, or a
                    // just-released holder still inside the grace window (covers
                    // UDP tail frames lagging the reliable PttUp). See
                    // RELEASE_GRACE.
                    let allowed = room.duplex.is_full()
                        || should_relay(
                            room.holder.as_deref(),
                            room.last_released
                                .as_ref()
                                .map(|(id, at)| (id.as_str(), at.elapsed())),
                            &sender_id,
                            RELEASE_GRACE,
                        );
                    if !allowed {
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
                Some((plaintext, sender_audio_id, targets))
            }
        };

        let Some((plaintext, sender_audio_id, targets)) = dispatch else {
            continue;
        };

        // Re-encrypt for each peer with their own session key and
        // outbound seq, then send. The send loop runs *outside* the
        // registry lock so a slow network path can't stall the
        // entire relay.
        let aad = toki_proto::wire::s2c_aad(version, sender_audio_id);
        for target in targets {
            let cipher = ChaCha20Poly1305::new(GenericArray::from_slice(&target.key));
            let nonce_bytes = build_nonce(target.seq);
            let nonce = Nonce::from_slice(&nonce_bytes);
            let mut buf_ct = plaintext.clone();
            // AAD = the sender's codec version + routing id, both stamped
            // into the S2C header below — a tampered header fails the tag.
            let tag = match cipher.encrypt_in_place_detached(nonce, &aad, &mut buf_ct) {
                Ok(t) => t,
                Err(e) => {
                    warn!(error = %e, "AEAD encrypt failed for outbound peer");
                    continue;
                }
            };
            // S2C layout: version (1) | sender_id (4 LE) | seq (8) | tag (16) | ciphertext
            let mut pkt = Vec::with_capacity(HEADER_LEN_S2C + buf_ct.len());
            pkt.push(version);
            pkt.extend_from_slice(&sender_audio_id.to_le_bytes());
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

/// Seal a `VERSION_PONG` reply echoing a keepalive's RTT probe. Same
/// S2C framing + AEAD as an audio packet (`VERSION_PONG` as both the
/// header byte and the AAD, the session key, a fresh outbound seq as
/// nonce), so the client verifies and replay-checks it on the existing
/// inbound path. Returns `None` only if the AEAD encrypt fails (never,
/// in practice).
fn seal_pong(session_key: &[u8], out_seq: u64, ping_id: u64, send_ts: u64) -> Option<Vec<u8>> {
    let cipher = ChaCha20Poly1305::new(Key::from_slice(session_key));
    let nonce_bytes = build_nonce(out_seq);
    let nonce = Nonce::from_slice(&nonce_bytes);
    let mut buf_ct = toki_proto::wire::encode_ping(ping_id, send_ts).to_vec();
    let tag = cipher
        .encrypt_in_place_detached(nonce, &[VERSION_PONG], &mut buf_ct)
        .ok()?;
    debug_assert_eq!(buf_ct.len(), PING_LEN);
    let mut pkt = Vec::with_capacity(HEADER_LEN_S2C + buf_ct.len());
    pkt.push(VERSION_PONG);
    pkt.extend_from_slice(&out_seq.to_le_bytes());
    pkt.extend_from_slice(tag.as_slice());
    pkt.extend_from_slice(&buf_ct);
    Some(pkt)
}

/// Snapshot of what we need to send to each peer in the fan-out.
/// Collected under the registry lock; the actual `send_to` happens
/// outside so a slow recipient can't block the relay.
struct PeerTarget {
    addr: SocketAddr,
    key: [u8; toki_proto::wire::MAC_KEY_LEN],
    seq: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    const G: Duration = Duration::from_millis(200);

    #[test]
    fn relays_for_the_current_holder() {
        assert!(should_relay(Some("a"), None, "a", G));
    }

    // ── payload shape (regression: keepalive RTT probe must pass) ──

    #[test]
    fn keepalive_accepts_empty_and_probe_payloads() {
        // The bug: a probe-carrying keepalive was dropped, so last_seen
        // never refreshed (reaper evicted the client ~30 s in) and no
        // pong was sent (RTT bars stayed gray). Both lengths must pass.
        assert!(payload_shape_ok(VERSION_KEEPALIVE, 0));
        assert!(payload_shape_ok(VERSION_KEEPALIVE, PING_LEN));
        // A wrong-sized keepalive payload is still rejected.
        assert!(!payload_shape_ok(VERSION_KEEPALIVE, PING_LEN - 1));
        assert!(!payload_shape_ok(VERSION_KEEPALIVE, FRAME_BYTES));
    }

    #[test]
    fn audio_payload_shapes() {
        assert!(payload_shape_ok(VERSION_AUDIO_PCM, FRAME_BYTES));
        assert!(!payload_shape_ok(VERSION_AUDIO_PCM, FRAME_BYTES - 2));
        assert!(payload_shape_ok(VERSION_AUDIO_OPUS, 1));
        assert!(payload_shape_ok(VERSION_AUDIO_OPUS, MAX_OPUS_PAYLOAD));
        assert!(!payload_shape_ok(VERSION_AUDIO_OPUS, 0));
        assert!(!payload_shape_ok(VERSION_AUDIO_OPUS, MAX_OPUS_PAYLOAD + 1));
    }

    #[test]
    fn unknown_version_with_payload_is_rejected() {
        // A client-sent PONG (or any unknown version) carrying a payload
        // is malformed/hostile and dropped; an empty one is harmless.
        assert!(!payload_shape_ok(VERSION_PONG, PING_LEN));
        assert!(!payload_shape_ok(99, 1));
        assert!(payload_shape_ok(99, 0));
    }

    #[test]
    fn prune_drops_stale_keeps_live_rate_entries() {
        // The leak: spoofed/dead-token entries (key = raw wire token) are
        // never reaped and a window reset doesn't remove them, so the map
        // grows unbounded under a distinct-token flood. The sweep must
        // drop entries whose window has gone stale and keep active ones.
        let now = Instant::now();
        let mut map: HashMap<[u8; TOKEN_LEN], RateState> = HashMap::new();
        // Live: window opened "now" — still inside RATE_WINDOW.
        map.insert(
            [1u8; TOKEN_LEN],
            RateState {
                window_start: now,
                packets: 5,
            },
        );
        // Stale: window opened well over a window ago (a token that sent
        // once and vanished, or a spoofed flood token).
        map.insert(
            [2u8; TOKEN_LEN],
            RateState {
                window_start: now - (RATE_WINDOW + Duration::from_millis(500)),
                packets: 1,
            },
        );
        // Exactly at the boundary counts as stale (`>= RATE_WINDOW` resets
        // a live entry anyway, so a boundary entry carries no live budget).
        map.insert(
            [3u8; TOKEN_LEN],
            RateState {
                window_start: now - RATE_WINDOW,
                packets: 1,
            },
        );

        prune_rate_state(&mut map, now);

        assert!(map.contains_key(&[1u8; TOKEN_LEN]), "live entry dropped");
        assert!(!map.contains_key(&[2u8; TOKEN_LEN]), "stale entry kept");
        assert!(!map.contains_key(&[3u8; TOKEN_LEN]), "boundary entry kept");
        assert_eq!(map.len(), 1);
    }

    #[test]
    fn drops_audio_from_a_non_holder() {
        // Someone else holds the floor — sender "b" is barging in.
        assert!(!should_relay(Some("a"), None, "b", G));
    }

    #[test]
    fn relays_just_released_holder_within_grace() {
        // Floor free, "a" released 50 ms ago: their UDP tail still flows.
        assert!(should_relay(
            None,
            Some(("a", Duration::from_millis(50))),
            "a",
            G
        ));
    }

    #[test]
    fn drops_released_holder_after_grace_expires() {
        assert!(!should_relay(
            None,
            Some(("a", Duration::from_millis(250))),
            "a",
            G
        ));
    }

    #[test]
    fn grace_only_helps_the_one_who_released() {
        // "a" just released, but "b" is the one sending — no free ride.
        assert!(!should_relay(
            None,
            Some(("a", Duration::from_millis(10))),
            "b",
            G
        ));
    }

    #[test]
    fn new_holder_voids_a_previous_grace() {
        // holder is Some(b) (b just grabbed/preempted the floor); a's
        // residual tail must not be forwarded even though a released
        // moments ago. The grace arm requires holder.is_none().
        assert!(!should_relay(
            Some("b"),
            Some(("a", Duration::from_millis(10))),
            "a",
            G
        ));
    }

    // ── Broadcast fan-out selection ───────────────────────────────────

    /// Helper that mirrors the broadcast target-collection logic from `run`:
    /// when `broadcast_active == Some(sender_id)`, collect every peer ID
    /// that has an `audio_addr` set, excluding the sender.
    fn collect_broadcast_targets(
        registry: &crate::state::Registry,
        sender_id: &str,
    ) -> Vec<String> {
        let is_broadcaster = registry.broadcast_active.as_deref() == Some(sender_id);
        if !is_broadcaster {
            return Vec::new();
        }
        let peer_ids: Vec<String> = registry
            .clients
            .keys()
            .filter(|id| id.as_str() != sender_id)
            .cloned()
            .collect();
        peer_ids
            .into_iter()
            .filter(|id| {
                registry
                    .clients
                    .get(id)
                    .and_then(|c| c.audio_addr)
                    .is_some()
            })
            .collect()
    }

    #[test]
    fn broadcast_fans_out_to_all_clients() {
        use crate::state::{Client, Registry, Room, TOKEN_HASH_LEN};
        use std::net::{Ipv4Addr, SocketAddr};
        use std::time::Instant;

        let addr_of = |b: u8| -> SocketAddr {
            SocketAddr::new(std::net::IpAddr::V4(Ipv4Addr::new(10, 0, 0, b)), 4000)
        };

        let mk = |id: &str, freq: &str, addr: Option<SocketAddr>| -> Client {
            let mut c = Client {
                id: id.to_string(),
                display_name: id.to_string(),
                audio_token_hash: [0u8; TOKEN_HASH_LEN],
                audio_mac_key: [0u8; toki_proto::wire::MAC_KEY_LEN],
                audio_last_seq: 0,
                audio_outbound_seq: 1,
                audio_id: 0,
                audio_addr: addr,
                events_tx: None,
                current_frequency: Some(freq.to_string()),
                priority_freq: None,
                last_seen: Instant::now(),
                connected_at: Instant::now(),
                expected_ip: None,
                identity: None,
                muted: false,
                can_global_broadcast: false,
                quality: None,
            };
            c.can_global_broadcast = id == "broadcaster";
            c
        };

        let mut reg = Registry::default();

        // Broadcaster in room "A".
        reg.clients.insert(
            "broadcaster".into(),
            mk("broadcaster", "A", Some(addr_of(1))),
        );

        // Two clients in room "A".
        reg.clients
            .insert("alice".into(), mk("alice", "A", Some(addr_of(2))));
        reg.clients
            .insert("bob".into(), mk("bob", "A", Some(addr_of(3))));

        // One client in a different room "B".
        reg.clients
            .insert("carol".into(), mk("carol", "B", Some(addr_of(4))));

        // One client with no UDP addr yet (never sent a packet).
        reg.clients.insert(
            "dave".into(),
            mk("dave", "B", None), // no audio_addr
        );

        // Set up rooms.
        let mut room_a = Room::default();
        room_a.members = vec!["broadcaster".into(), "alice".into(), "bob".into()];
        reg.rooms.insert("A".into(), room_a);
        let mut room_b = Room::default();
        room_b.members = vec!["carol".into(), "dave".into()];
        reg.rooms.insert("B".into(), room_b);

        reg.broadcast_active = Some("broadcaster".into());

        let mut targets = collect_broadcast_targets(&reg, "broadcaster");
        targets.sort(); // HashMap iteration order is non-deterministic.

        // Should reach alice, bob, carol (have audio_addr) but NOT broadcaster,
        // NOT dave (no addr).
        assert_eq!(targets, vec!["alice", "bob", "carol"]);

        // Sanity: a non-broadcaster sender produces no broadcast targets.
        assert!(collect_broadcast_targets(&reg, "alice").is_empty());
    }
}
