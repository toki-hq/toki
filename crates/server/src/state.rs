use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::{mpsc, Mutex, RwLock};

use toki_proto::v1::Event;

/// Per-frequency duplex behaviour. `Half` (the default) is the classic
/// single-PTT-floor walkie-talkie; `Full` lets several members transmit
/// at once and clients mix the concurrent streams.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum DuplexMode {
    #[default]
    Half,
    Full,
}

impl DuplexMode {
    /// Decode the wire/db integer (0 = half, 1 = full); anything else
    /// falls back to the safe default (half).
    pub fn from_u32(v: u32) -> Self {
        match v {
            1 => DuplexMode::Full,
            _ => DuplexMode::Half,
        }
    }

    pub fn as_u32(self) -> u32 {
        match self {
            DuplexMode::Half => 0,
            DuplexMode::Full => 1,
        }
    }

    pub fn is_full(self) -> bool {
        matches!(self, DuplexMode::Full)
    }
}

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
    /// Compact per-session id stamped into the S2C audio header so a
    /// receiver can route this sender's packets to their own decoder +
    /// jitter buffer when mixing concurrent talkers on a full-duplex
    /// channel. Assigned once at register from `Registry::alloc_audio_id`;
    /// opaque (not an identity), unique among live sessions.
    pub audio_id: u32,
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
    /// Duplex behaviour of this channel. Cached here (the source of truth
    /// is `SharedDuplexModes` + the `channel_modes` db table) so the audio
    /// relay can branch without a second lock. Initialised from the shared
    /// map on room creation and updated when an admin changes the mode.
    pub duplex: DuplexMode,
    /// Walkie-talkie lock: client_id of the current PTT holder. At most one
    /// member may transmit at a time. `None` means the room is free.
    /// **Half-duplex only** — unused on full-duplex channels.
    pub holder: Option<String>,
    /// On a **full-duplex** channel, the set of members currently keying
    /// (PTT held). There's no single floor; this drives the multi-talker
    /// roster indicators. Empty on half-duplex channels.
    pub active_talkers: HashSet<String>,
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
    /// Monotonic source for per-session `Client.audio_id`. Starts at 0;
    /// `alloc_audio_id` pre-increments so ids begin at 1 (0 is never a
    /// live sender). Wraps after 2^32 sessions — astronomically beyond
    /// any real uptime, and old ids are long gone by then.
    next_audio_id: u32,
}

impl Registry {
    /// Allocate the next per-session audio routing id (≥ 1).
    pub fn alloc_audio_id(&mut self) -> u32 {
        self.next_audio_id = self.next_audio_id.wrapping_add(1);
        if self.next_audio_id == 0 {
            self.next_audio_id = 1;
        }
        self.next_audio_id
    }
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

/// Admin-assigned per-frequency duplex modes (canonical frequency →
/// [`DuplexMode`]), shared between the admin mutation handlers (writers)
/// and the signaling service (reader, on `Join` / `ChangeFrequency`).
/// Loaded from the `channel_modes` table at startup. Same rationale as
/// [`SharedChannelNames`]: modes persist independently of room occupancy
/// and reads dominate, so it lives off the registry `Mutex`. Only
/// non-default (full-duplex) frequencies need an entry; an absent key is
/// half-duplex.
pub type SharedDuplexModes = Arc<RwLock<HashMap<String, DuplexMode>>>;

/// Build a shared duplex-mode map from an initial snapshot (typically
/// `AdminDb::load_channel_modes` at boot, or empty for headless runs).
pub fn shared_duplex_modes(initial: HashMap<String, DuplexMode>) -> SharedDuplexModes {
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
