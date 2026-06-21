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
    /// Verified keypair identity, when the client presented one at
    /// register (see `crate::identity::verify_register`). `None` for
    /// pre-identity clients and identity-less registers. Denormalized
    /// onto the session so snapshots + audit lines never have to
    /// touch the shared identity map.
    pub identity: Option<ClientIdentity>,
    /// Server-side mute: while `true`, the relay's speak-gate refuses
    /// this session's PTT presses (see [`Client::can_speak`]). The
    /// member stays connected and keeps receiving the channel; they
    /// just can't transmit. Set by the admin `SetMute` RPC, cleared on
    /// disconnect — session-scoped, like `priority_freq`. The durable,
    /// identity-keyed tier is a deliberate later slice.
    pub muted: bool,
    /// Admin-granted global-broadcast capability. When true, this session
    /// may key the broadcast PTT, which simultaneously seizes every occupied
    /// room. Session-scoped: cleared on disconnect. Only one client holds it
    /// at a time (admin RPC enforces); the broadcast itself is additionally
    /// serialized by `Registry.broadcast_active`.
    pub can_global_broadcast: bool,
    /// Most-recent connection-quality sample the client reported via
    /// `Signaling.ReportConnectionQuality`. `None` until the first
    /// report lands — the server can't measure these itself (only the
    /// receiver sees its own loss/jitter, and RTT is a client-stamped
    /// round trip), so it just stores what the client pushes up for the
    /// admin dashboard.
    pub quality: Option<ConnQuality>,
}

impl Default for Client {
    fn default() -> Self {
        Client {
            id: String::new(),
            display_name: String::new(),
            audio_token_hash: [0u8; TOKEN_HASH_LEN],
            audio_mac_key: [0u8; toki_proto::wire::MAC_KEY_LEN],
            audio_last_seq: 0,
            audio_outbound_seq: 1,
            audio_id: 0,
            audio_addr: None,
            events_tx: None,
            current_frequency: None,
            priority_freq: None,
            last_seen: std::time::Instant::now(),
            connected_at: std::time::Instant::now(),
            expected_ip: None,
            identity: None,
            muted: false,
            can_global_broadcast: false,
            quality: None,
        }
    }
}

/// Client-reported connection-quality metrics, denormalized onto the
/// session for the admin snapshot. All as-of the last report.
#[derive(Clone, Copy, Debug, Default)]
pub struct ConnQuality {
    /// Smoothed round-trip time, milliseconds (keepalive/pong probe).
    pub rtt_ms: u32,
    /// Inter-arrival jitter, milliseconds.
    pub jitter_ms: u32,
    /// Inbound packet loss, percent ×100 (250 = 2.50%).
    pub loss_pct_centi: u32,
}

impl Client {
    /// The relay-side **speak gate**: may this session take/hold the
    /// PTT floor right now? Today the only veto is an admin mute, but
    /// this is intentionally the single chokepoint every "can this
    /// member transmit" decision flows through — both the signaling
    /// PTT-arbitration path and the UDP relay backstop call it, and
    /// No-Talk channels (default-deny + per-member grant) will extend
    /// the same check rather than bolting on a parallel one.
    pub fn can_speak(&self) -> bool {
        !self.muted
    }
}

/// The session-facing slice of a verified identity — exactly what
/// admin snapshots and audit lines need.
#[derive(Clone, Debug)]
pub struct ClientIdentity {
    /// Human-readable identity string — the 8-char base32 fingerprint
    /// of the public key, e.g. `7Q4XF9KB`. Purely key-derived, so it's
    /// stable across renames, sessions, and machines.
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
    /// Global broadcast lock. `Some(client_id)` while a broadcast is live,
    /// `None` when idle. Serializes concurrent broadcast attempts (first-come
    /// wins). On Registry so the audio relay + signaling handler read it under
    /// the existing registry lock with no extra synchronization.
    pub broadcast_active: Option<String>,
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

    /// Is `callsign` already used by a connected client other than
    /// `except` (a `client_id` to skip — e.g. the subject of a rename,
    /// who legitimately keeps their own name)?
    ///
    /// `own_identity` exempts a holder that is *the same identity* as the
    /// caller: a keypair-backed client reconnecting (a fresh session id,
    /// same identity) before its old session is reaped should keep its
    /// own callsign rather than be locked out by its own ghost. `None`
    /// (identity-less register/rename) never matches this exemption.
    ///
    /// Case-insensitive, since callsigns are uppercased client-side and
    /// `ECHO-1` / `echo-1` should collide. Drives the unique-callsign
    /// gate on register and admin rename. Linear scan over the live
    /// clients — fine at the hundreds-of-peers scale Toki targets, and
    /// only runs on the two cold paths (register, rename), never per
    /// audio packet.
    pub fn callsign_taken(
        &self,
        callsign: &str,
        except: Option<&str>,
        own_identity: Option<&str>,
    ) -> bool {
        let want = callsign.to_lowercase();
        self.clients.iter().any(|(id, c)| {
            if Some(id.as_str()) == except {
                return false;
            }
            if c.display_name.to_lowercase() != want {
                return false;
            }
            // Same-identity reconnect keeps its name (not a collision).
            !matches!(
                (own_identity, c.identity.as_ref()),
                (Some(mine), Some(theirs)) if theirs.pubkey_hex == mine
            )
        })
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

/// Channel-wide mutes — the set of canonical frequencies on which no one
/// may transmit (see the admin `SetChannelMute` RPC). Written by the
/// admin handlers, read by the signaling PTT path and the UDP relay's
/// speak-gate (keyed by the sender's current frequency). Same
/// writer/reader split and off-`Registry` rationale as
/// [`SharedChannelNames`]: a mute persists independently of room
/// occupancy (you can mute an empty channel), and reads dominate.
/// Hydrated from the `channel_mutes` table at boot.
pub type SharedChannelMutes = Arc<RwLock<std::collections::HashSet<String>>>;

/// Build a shared channel-mute set from an initial snapshot (typically
/// `AdminDb::load_channel_mutes` at boot, or empty for tests).
pub fn shared_channel_mutes(initial: std::collections::HashSet<String>) -> SharedChannelMutes {
    Arc::new(RwLock::new(initial))
}

/// Everything this server remembers about one client identity,
/// keyed by the pubkey hex in [`SharedIdentities`] and mirrored to
/// the `identities` table by the admin task.
#[derive(Clone, Debug)]
pub struct IdentityRecord {
    /// Human-readable identity string (`7Q4XF9KB` — the 8-char key
    /// fingerprint). Derived from the pubkey and stored so audit rows
    /// can be joined against it without re-deriving.
    pub display_id: String,
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

/// One active identity ban, keyed by the banned pubkey hex in
/// [`SharedBans`] and mirrored to the `bans` table.
#[derive(Clone, Debug)]
pub struct BanRecord {
    /// 8-char fingerprint of the banned key, for display.
    pub display_id: String,
    /// Display name the session used when it was banned. Display aid
    /// only — names are freely chosen and may be reused by others.
    pub last_callsign: String,
    /// When non-empty, the machine tier is banned too: ANY identity
    /// presenting this machine hash at register is rejected, so a
    /// config wipe (fresh key, same machine) stays banned.
    pub machine_hash: String,
    /// Operator-supplied reason, echoed to the banned client in the
    /// register rejection.
    pub reason: String,
    /// Admin username that issued the ban.
    pub banned_by: String,
    /// Unix seconds the ban was issued.
    pub banned_at: i64,
}

/// Active bans (banned pubkey hex → record). Written by the admin
/// gRPC handlers (ban / lift — they own the db), read by the signaling
/// `Register` gate. Same writer/reader split as [`SharedChannelNames`];
/// hydrated from the `bans` table at boot.
pub type SharedBans = Arc<RwLock<HashMap<String, BanRecord>>>;

/// Build a shared ban map from an initial snapshot (typically
/// `AdminDb::load_bans` at boot, or empty for tests).
pub fn shared_bans(initial: HashMap<String, BanRecord>) -> SharedBans {
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
