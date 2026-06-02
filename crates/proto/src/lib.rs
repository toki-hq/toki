pub mod v1 {
    tonic::include_proto!("toki.v1");
}

/// Admin control-plane service (gRPC-Web), consumed by the web panel SPA.
/// Separate package from the signaling `v1` so the two surfaces evolve
/// independently.
pub mod admin {
    pub mod v1 {
        tonic::include_proto!("toki.admin.v1");
    }
}

/// Wire format constants shared by the client and the server's UDP audio relay.
///
/// Every UDP packet is encrypted + authenticated with ChaCha20-Poly1305 AEAD
/// keyed by `RegisterResponse.audio_mac_key`. The seq doubles as the AEAD
/// nonce (zero-padded to 12 bytes) and is also enforced as a strict-monotonic
/// counter for replay protection. The version byte is bound as associated
/// data so an attacker can't tweak a keepalive into an audio frame.
///
/// Client → server packet layout (`HEADER_LEN_C2S` = 41 bytes header):
///
/// ```text
///   [0..16]    16-byte session token (from `RegisterResponse.audio_token`)
///   [16]       1-byte packet kind (see `VERSION_*`); AEAD associated data
///   [17..25]   8-byte little-endian monotonic sequence number; doubles as AEAD nonce
///   [25..41]   16-byte Poly1305 tag (AEAD)
///   [41..]     ChaCha20 ciphertext of the payload — for audio,
///              little-endian i16 PCM samples; zero bytes for keepalives
/// ```
///
/// Server → peer packet layout (`HEADER_LEN_S2C` = 29 bytes header):
///
/// ```text
///   [0]        1-byte codec version (the *sender's* VERSION_AUDIO_*); AEAD AAD
///   [1..5]     4-byte little-endian sender routing id; AEAD AAD
///   [5..13]    8-byte little-endian monotonic seq, peer-specific; doubles as nonce
///   [13..29]   16-byte Poly1305 tag
///   [29..]     ChaCha20 ciphertext of the audio payload (PCM or Opus)
/// ```
///
/// No token in the S2C direction: the client's socket is `connect()`-ed
/// to exactly one server endpoint and trusts everything that arrives;
/// the AEAD tag is the actual authenticator. The codec version + sender
/// id are bound as associated data (see [`wire::s2c_aad`]) so neither can
/// be tampered, and the sender id lets the receiver demux + mix several
/// concurrent talkers on a full-duplex channel.
pub mod wire {
    pub const TOKEN_LEN: usize = 16;
    /// Per-packet monotonic sequence width, in bytes (le-encoded u64).
    pub const SEQ_LEN: usize = 8;
    /// Poly1305 tag length — fixed by ChaCha20-Poly1305.
    pub const TAG_LEN: usize = 16;
    /// Symmetric session key handed back in `RegisterResponse.audio_mac_key`.
    /// 32 bytes is the ChaCha20-Poly1305 key width.
    pub const MAC_KEY_LEN: usize = 32;
    /// ChaCha20-Poly1305 nonce length. We zero-pad the seq into the
    /// trailing 8 bytes; the leading 4 bytes are constant zero.
    pub const NONCE_LEN: usize = 12;

    /// Per-sender routing id stamped into the S2C header, in bytes
    /// (le-encoded u32). The server assigns each session a compact id at
    /// register; the receiver uses it purely to route each packet to the
    /// right per-sender decoder + jitter buffer when several peers talk
    /// at once on a full-duplex channel. It is *not* an identity — the
    /// roster's talking indicators come from the gRPC `Ptt` events.
    pub const SENDER_ID_LEN: usize = 4;

    /// Client→server header length (token + version + seq + tag).
    pub const HEADER_LEN_C2S: usize = TOKEN_LEN + 1 + SEQ_LEN + TAG_LEN;
    /// Server→peer header length: `version(1) | sender_id(4) | seq(8) | tag(16)`.
    /// (No token — the client trusts everything on its `connect()`-ed
    /// socket and the AEAD tag is the authenticator.)
    ///
    /// - **version** tells the receiver which codec the relayed payload
    ///   uses (PCM vs Opus); the server stamps the *sender's* version so
    ///   mixed-codec senders interoperate per-packet.
    /// - **sender_id** routes the packet to a per-sender decoder/jitter
    ///   buffer so a receiver can mix concurrent talkers on a full-duplex
    ///   channel (on a half-duplex channel there's only ever one).
    ///
    /// Both `version` and `sender_id` are folded into the AEAD associated
    /// data (see [`s2c_aad`]), so a tampered header fails the tag check.
    pub const HEADER_LEN_S2C: usize = 1 + SENDER_ID_LEN + SEQ_LEN + TAG_LEN;

    /// Build the S2C AEAD associated data from the packet's plaintext
    /// header fields (codec version + sender routing id). The relay
    /// (encrypt) and the receiver (decrypt) must construct this
    /// identically.
    pub fn s2c_aad(version: u8, sender_id: u32) -> [u8; 1 + SENDER_ID_LEN] {
        let mut aad = [0u8; 1 + SENDER_ID_LEN];
        aad[0] = version;
        aad[1..].copy_from_slice(&sender_id.to_le_bytes());
        aad
    }

    /// Back-compat alias for code that still wants the inbound header
    /// length — kept until call sites migrate to the explicit name.
    #[deprecated(note = "use HEADER_LEN_C2S or HEADER_LEN_S2C explicitly")]
    pub const HEADER_LEN: usize = HEADER_LEN_C2S;

    /// Empty payload packet used to register the client's UDP source
    /// address with the server (and keep NAT mappings alive). Not
    /// forwarded to peers. Still carries a sequence + tag so an off-
    /// path attacker can't replay one to keep a session alive on the
    /// legitimate client's behalf.
    pub const VERSION_KEEPALIVE: u8 = 0;

    /// Raw PCM audio frame, little-endian i16, mono, 48 kHz.
    pub const VERSION_AUDIO_PCM: u8 = 1;

    /// Opus-encoded audio frame, 48 kHz mono, 10 ms. Variable length;
    /// the server relays the opaque payload (it never decodes Opus).
    pub const VERSION_AUDIO_OPUS: u8 = 2;

    pub const SAMPLE_RATE_HZ: u32 = 48_000;
    /// 10 ms of mono audio at 48 kHz — the PCM frame size and the
    /// capture/chunking granularity.
    pub const FRAME_SAMPLES: usize = 480;
    pub const FRAME_BYTES: usize = FRAME_SAMPLES * 2;

    /// Opus encodes 10 ms frames (480 samples @ 48 kHz) — one frame per
    /// captured mic frame, so the outbound path needs no buffering and
    /// adds no framing latency over the PCM path (the codec still cuts
    /// bandwidth ~20×). A 20 ms frame would halve the packet rate but
    /// add ~10 ms of mouth-to-ear delay; 10 ms favours low latency.
    pub const OPUS_FRAME_SAMPLES: usize = 480;
    /// Generous cap on a single Opus frame's bytes. At ≤32 kbps/10 ms a
    /// frame is ~50 bytes; 512 leaves headroom for higher rates / FEC
    /// while still bounding the per-packet allocation.
    pub const MAX_OPUS_PAYLOAD: usize = 512;

    /// Largest UDP datagram we ever send/receive. The PCM frame (960 B)
    /// dominates the Opus payload, so the C2S PCM packet is the ceiling
    /// (41 B header). The S2C header is 29 B (no token, +sender_id), so
    /// an S2C PCM packet (989 B) still fits under this. Sized off C2S.
    pub const MAX_AUDIO_PACKET: usize = HEADER_LEN_C2S + FRAME_BYTES;

    /// True for the two forwardable audio codecs (not keepalive).
    pub fn is_audio(version: u8) -> bool {
        version == VERSION_AUDIO_PCM || version == VERSION_AUDIO_OPUS
    }

    /// Build the ChaCha20-Poly1305 nonce for a packet with the given
    /// seq. Layout: [0; 4] || seq.to_le_bytes(). Constant prefix
    /// because seq is already monotonic per (key, direction) and
    /// gives 2^64 unique nonces per session — more than will ever
    /// be reached.
    pub fn build_nonce(seq: u64) -> [u8; NONCE_LEN] {
        let mut nonce = [0u8; NONCE_LEN];
        nonce[4..].copy_from_slice(&seq.to_le_bytes());
        nonce
    }
}

/// Protocol-version compatibility between a Toki client and server.
///
/// The UDP audio wire format and the gRPC contract can change across
/// **minor** versions (e.g. the v0.3.0 Opus work added a codec byte to
/// the server→peer packet header). A client and server on different
/// MAJOR.MINOR can still complete the gRPC handshake — protobuf fields
/// are additive — but their audio would silently fail to decode. To
/// turn that dead air into an explicit, actionable error, the server
/// gates `Register` on a matching MAJOR.MINOR. **Patch** releases are
/// guaranteed wire-compatible, so they interoperate freely.
pub mod version {
    /// Extract the leading `major.minor` from a semantic-version string,
    /// ignoring patch, pre-release (`-rc.1`), and build (`+meta`) parts.
    /// Returns `None` if the string doesn't start with two dot-separated
    /// integers (including the empty string).
    pub fn major_minor(v: &str) -> Option<(u64, u64)> {
        // Drop any pre-release / build metadata before splitting.
        let core = v.split(['-', '+']).next().unwrap_or(v);
        let mut parts = core.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next()?.parse().ok()?;
        Some((major, minor))
    }

    /// True iff `client` and `server` share the same MAJOR.MINOR. Patch
    /// may differ. Either side failing to parse (an empty / malformed
    /// version, as sent by clients predating this gate) is treated as
    /// **incompatible** — those builds predate the current wire format.
    pub fn compatible(server: &str, client: &str) -> bool {
        match (major_minor(server), major_minor(client)) {
            (Some(s), Some(c)) => s == c,
            _ => false,
        }
    }
}

#[cfg(test)]
mod wire_tests {
    use super::wire::*;

    #[test]
    fn s2c_header_layout_is_29_bytes() {
        // version(1) + sender_id(4) + seq(8) + tag(16)
        assert_eq!(HEADER_LEN_S2C, 1 + 4 + 8 + 16);
        assert_eq!(SENDER_ID_LEN, 4);
        // The S2C PCM packet must still fit under the global ceiling.
        assert!(HEADER_LEN_S2C + FRAME_BYTES <= MAX_AUDIO_PACKET);
    }

    #[test]
    fn s2c_aad_round_trips_version_and_sender() {
        let aad = s2c_aad(VERSION_AUDIO_OPUS, 0xDEAD_BEEF);
        assert_eq!(aad[0], VERSION_AUDIO_OPUS);
        assert_eq!(
            u32::from_le_bytes([aad[1], aad[2], aad[3], aad[4]]),
            0xDEAD_BEEF
        );
        // Distinct (version, sender) pairs yield distinct AAD, so a
        // packet resealed for one stream can't be replayed as another.
        assert_ne!(
            s2c_aad(VERSION_AUDIO_PCM, 1),
            s2c_aad(VERSION_AUDIO_OPUS, 1)
        );
        assert_ne!(
            s2c_aad(VERSION_AUDIO_OPUS, 1),
            s2c_aad(VERSION_AUDIO_OPUS, 2)
        );
    }
}

#[cfg(test)]
mod version_tests {
    use super::version::{compatible, major_minor};

    #[test]
    fn parses_major_minor_ignoring_patch_and_metadata() {
        assert_eq!(major_minor("0.3.1"), Some((0, 3)));
        assert_eq!(major_minor("1.4.0-rc.2"), Some((1, 4)));
        assert_eq!(major_minor("2.10.5+build.7"), Some((2, 10)));
    }

    #[test]
    fn rejects_unparseable_versions() {
        assert_eq!(major_minor(""), None);
        assert_eq!(major_minor("0"), None);
        assert_eq!(major_minor("x.y.z"), None);
    }

    #[test]
    fn same_major_minor_is_compatible_regardless_of_patch() {
        assert!(compatible("0.3.0", "0.3.0"));
        assert!(compatible("0.3.0", "0.3.9"));
        assert!(compatible("0.3.7", "0.3.1"));
    }

    #[test]
    fn differing_major_or_minor_is_incompatible() {
        assert!(!compatible("0.3.0", "0.4.0")); // minor
        assert!(!compatible("0.3.0", "1.3.0")); // major
    }

    #[test]
    fn empty_or_garbage_client_is_incompatible() {
        assert!(!compatible("0.3.0", "")); // pre-gate client
        assert!(!compatible("0.3.0", "garbage"));
    }
}
