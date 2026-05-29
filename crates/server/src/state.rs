use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::{mpsc, Mutex, RwLock};

use toki_proto::v1::Event;

/// Length of the BLAKE3 digest we use to key the token table. The
/// full 32-byte BLAKE3 output is overkill for a 16-byte preimage —
/// truncating to 16 bytes preserves preimage resistance well past
/// the ~2^128 cost we'd need to attack anyway.
pub const TOKEN_HASH_LEN: usize = 16;

#[derive(Clone)]
pub struct Client {
    pub id: String,
    pub display_name: String,
    /// BLAKE3 of the raw session token that the *client* receives
    /// from `RegisterResponse.audio_token`. We don't keep the
    /// preimage anywhere — a process memory snapshot leaks only the
    /// hash, which is useless against the UDP relay's hash-then-
    /// compare check.
    pub audio_token_hash: [u8; TOKEN_HASH_LEN],
    /// Symmetric BLAKE3-keyed-hash key used to authenticate this
    /// session's UDP packets. The client receives the same key in
    /// `RegisterResponse.audio_mac_key` and uses it to MAC every
    /// outbound packet. Lives only in server memory + the client's
    /// runtime; never persisted.
    pub audio_mac_key: [u8; toki_proto::wire::MAC_KEY_LEN],
    /// Highest sequence number we've accepted from this session
    /// (client → server direction). Strict-monotonic replay
    /// protection: incoming packets must carry `seq > audio_last_seq`
    /// to be forwarded. UDP reordering will drop the occasional
    /// out-of-order frame, which the playback path already tolerates
    /// as ordinary packet loss.
    pub audio_last_seq: u64,
    /// Monotonic seq we'll use for the *next* outbound packet sent
    /// to this peer (server → peer direction). Independent of the
    /// inbound counter — the AEAD nonce space is per-direction, and
    /// the peer's playback-side replay check uses this directly.
    pub audio_outbound_seq: u64,
    pub audio_addr: Option<SocketAddr>,
    pub events_tx: Option<mpsc::Sender<Event>>,
    /// The frequency room the client is currently in. `None` between
    /// `Register` and `Join`, and again after `Leave`. We key the
    /// audio relay's forwarding fan-out off this — silent clients on
    /// frequency A never receive a sender's voice on frequency B.
    pub current_frequency: Option<String>,
    /// Frequency on which an admin has elected this session as a
    /// *priority* speaker, if any. Priority is **per-channel** and
    /// **per-session**: it's effective only while
    /// `current_frequency == priority_freq`, goes dormant if the
    /// member tunes elsewhere (re-activating if they return), and
    /// vanishes when the session ends — there is no persistent
    /// identity to anchor it to. A priority press preempts a
    /// *non-priority* holder on the same channel; priority-vs-priority
    /// is first-come (see `push_to_talk`). `None` for ordinary
    /// members.
    pub priority_freq: Option<String>,
    /// Refreshed on every UDP packet from this client (keepalive or audio).
    /// The reaper evicts clients whose `last_seen` is older than the
    /// configured timeout — see `reaper`. Internal lifecycle signal —
    /// the admin panel surfaces `connected_at` instead, since a value
    /// that resets every keepalive (~1-2 s) is useless for "how long
    /// has this client been here".
    pub last_seen: Instant,
    /// Wall-clock instant the gRPC `Register` call landed. Never
    /// updated for the life of the session, so `now - connected_at`
    /// gives the operator a monotonically-increasing session age.
    /// Mirrors the "logged in since X" stat in chat-style admin
    /// surfaces.
    pub connected_at: Instant,
    /// IP the gRPC `Register` call came from (`None` only on
    /// transports that don't expose one, e.g. Unix sockets). The
    /// audio relay enforces that incoming UDP packets bearing this
    /// client's token *must* originate from this IP — closes the
    /// "token-capture → audio hijack" path. The *port* is allowed to
    /// vary because NAT will usually pick a different one for UDP.
    pub expected_ip: Option<IpAddr>,
}

/// One frequency channel. Each holds its own member list and PTT lock —
/// frequencies are independent walkie-talkie channels that don't see
/// each other's traffic.
#[derive(Default)]
pub struct Room {
    pub members: Vec<String>,
    /// Walkie-talkie lock: client_id of the current PTT holder. At most one
    /// member may transmit at a time. `None` means the room is free.
    pub holder: Option<String>,
}

#[derive(Default)]
pub struct Registry {
    pub clients: HashMap<String, Client>,
    /// Rooms keyed by frequency string (e.g. `"446.05"`). Lazily
    /// inserted on first join; we don't pre-populate the full
    /// 41-channel grid because most are usually empty.
    pub rooms: HashMap<String, Room>,
    /// Token-hash → client-id map. Audio packets carry the raw
    /// 16-byte token; the relay hashes it and looks up here. The
    /// raw token is never persisted server-side after registration.
    pub tokens: HashMap<[u8; TOKEN_HASH_LEN], String>,
}

/// BLAKE3 the raw token and truncate to `TOKEN_HASH_LEN` bytes for
/// use as the `tokens` HashMap key.
pub fn hash_token(token: &[u8]) -> [u8; TOKEN_HASH_LEN] {
    let full = blake3::hash(token);
    let mut out = [0u8; TOKEN_HASH_LEN];
    out.copy_from_slice(&full.as_bytes()[..TOKEN_HASH_LEN]);
    out
}

pub type SharedRegistry = Arc<Mutex<Registry>>;

pub fn shared() -> SharedRegistry {
    Arc::new(Mutex::new(Registry::default()))
}

/// Admin-assigned channel names (canonical frequency → name), shared
/// between the admin mutation handlers (the writers) and the signaling
/// service (the reader, on `Join` / `ChangeFrequency`). Loaded from
/// `admin.db` at startup and kept in sync with the `channel_names`
/// table by the admin handlers.
///
/// A separate `RwLock` rather than living on the `Registry` because
/// names persist independently of room occupancy (a name outlives the
/// last member leaving) and reads dominate writes; keeping it off the
/// registry `Mutex` avoids holding that lock across a name lookup.
pub type SharedChannelNames = Arc<RwLock<HashMap<String, String>>>;

/// Build a shared channel-name map from an initial snapshot (typically
/// `AdminDb::load_channel_names` at boot, or empty for headless runs).
pub fn shared_channel_names(initial: HashMap<String, String>) -> SharedChannelNames {
    Arc::new(RwLock::new(initial))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_token_is_deterministic() {
        let token = b"some-16-byte-tok";
        assert_eq!(hash_token(token), hash_token(token));
    }

    #[test]
    fn hash_token_distinguishes_different_inputs() {
        assert_ne!(
            hash_token(b"alpha-tok-input!"),
            hash_token(b"bravo-tok-input!")
        );
    }

    #[test]
    fn hash_token_has_expected_length() {
        // The registry stores fixed-size keys — fast lookup +
        // makes the type signature unambiguous.
        let hash = hash_token(b"anything");
        assert_eq!(hash.len(), TOKEN_HASH_LEN);
    }

    #[test]
    fn registry_default_is_empty() {
        let r = Registry::default();
        assert!(r.clients.is_empty());
        assert!(r.rooms.is_empty());
        assert!(r.tokens.is_empty());
    }
}
