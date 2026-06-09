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
    /// Verified keypair identity, when the client presented one at
    /// register (see `crate::identity::verify_register`). `None` for
    /// pre-identity clients and identity-less registers. Denormalized
    /// onto the session so snapshots + audit lines never have to
    /// touch the shared identity map.
    pub identity: Option<ClientIdentity>,
}

/// The session-facing slice of a verified identity — exactly what
/// admin snapshots and audit lines need.
#[derive(Clone, Debug)]
pub struct ClientIdentity {
    /// Human-readable identity string, e.g. `COTON-7Q4XF9KB`. Derived
    /// from the *stored* first callsign for a returning identity, so
    /// a client can't rename its identity string later.
    pub display_id: String,
    /// Full ed25519 public key, lowercase hex — the canonical key.
    pub pubkey_hex: String,
    /// Salted machine-fingerprint hash presented this session (empty
    /// on platforms without a machine id).
    pub machine_hash: String,
    /// Unix seconds this identity was first seen by this server.
    pub first_seen: i64,
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
    /// The most recent holder and the instant they released the floor,
    /// set whenever `holder` transitions to `None`. The audio relay keeps
    /// forwarding *this* client's packets for a brief grace window after
    /// release: the PTT-release travels over the reliable gRPC stream and
    /// clears `holder` before the talker's final UDP voice frames arrive,
    /// so without a grace those tail frames would be dropped and the end
    /// of every transmission clipped. Cleared the moment a new holder
    /// takes the floor, so a fresh (or preempting) talker's audio never
    /// mixes with the previous holder's residual tail.
    pub last_released: Option<(String, std::time::Instant)>,
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

/// Everything this server remembers about one client identity,
/// keyed by the pubkey hex in [`SharedIdentities`] and mirrored to
/// the `identities` table by the admin task.
#[derive(Clone, Debug)]
pub struct IdentityRecord {
    /// Human-readable identity string (`COTON-7Q4XF9KB`). Derived
    /// once from `first_callsign` + pubkey and stored so audit rows
    /// can be joined against it without re-deriving.
    pub display_id: String,
    /// Callsign captured the first time this identity registered
    /// here — the display-id prefix, frozen forever.
    pub first_callsign: String,
    /// Display name used at the most recent register.
    pub last_callsign: String,
    /// Most recent machine-fingerprint hash (claimed; may be empty).
    pub machine_hash: String,
    /// Claimed provenance: the first session id any server ever
    /// assigned this identity. Recorded once, first non-empty wins.
    pub origin_client_id: String,
    /// Unix seconds of first / most recent register on this server.
    pub first_seen: i64,
    pub last_seen: i64,
    /// Source IP of the most recent register (empty when the
    /// transport exposed none).
    pub last_ip: String,
}

/// Identity records seen by this server (pubkey hex → record).
/// Hydrated from the `identities` table at boot by the admin task;
/// the signaling `Register` handler is the writer (merge + insert),
/// pushing each change to the admin task for persistence over the
/// identity channel — same split as the audit pipeline. Same
/// rationale as [`SharedChannelNames`] for living off the registry
/// `Mutex`: records outlive sessions and reads dominate.
pub type SharedIdentities = Arc<RwLock<HashMap<String, IdentityRecord>>>;

/// Build a shared identity map from an initial snapshot (typically
/// `AdminDb::load_identities` at boot, or empty for tests).
pub fn shared_identities(initial: HashMap<String, IdentityRecord>) -> SharedIdentities {
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
