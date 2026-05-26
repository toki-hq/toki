pub mod v1 {
    tonic::include_proto!("toki.v1");
}

/// Wire format constants shared by the client and the server's UDP audio relay.
///
/// Client → server packet layout:
///
/// ```text
///   [0..16]   16-byte session token (from `RegisterResponse.audio_token`)
///   [16]      1-byte packet kind (see `VERSION_*`)
///   [17..25]  8-byte little-endian monotonic sequence number
///   [25..33]  8-byte BLAKE3-keyed MAC over (version || seq || payload),
///             using `RegisterResponse.audio_mac_key` as the key
///   [33..]    payload — for audio, little-endian i16 PCM samples;
///             zero bytes for keepalives
/// ```
///
/// Server → other clients: just the PCM payload bytes. The server already
/// routed the packet, so peers don't need the header.
pub mod wire {
    pub const TOKEN_LEN: usize = 16;
    /// Per-packet monotonic sequence width, in bytes (le-encoded u64).
    pub const SEQ_LEN: usize = 8;
    /// MAC width — the first `MAC_LEN` bytes of a BLAKE3 keyed_hash.
    /// 64 bits is far past brute-force reach for a per-session key
    /// that exists only for the lifetime of a UDP flow.
    pub const MAC_LEN: usize = 8;
    pub const HEADER_LEN: usize = TOKEN_LEN + 1 + SEQ_LEN + MAC_LEN;

    /// Symmetric MAC key handed back in `RegisterResponse.audio_mac_key`.
    pub const MAC_KEY_LEN: usize = 32;

    /// Empty packet used to register the client's UDP source address with the
    /// server (and keep NAT mappings alive). Not forwarded to peers. Still
    /// carries a sequence + MAC so an off-path attacker can't replay one
    /// to keep a session alive on the legitimate client's behalf.
    pub const VERSION_KEEPALIVE: u8 = 0;

    /// Raw PCM audio frame, little-endian i16, mono, 48 kHz.
    pub const VERSION_AUDIO_PCM: u8 = 1;

    pub const SAMPLE_RATE_HZ: u32 = 48_000;
    /// 10 ms of mono audio at 48 kHz — keeps each UDP packet well under MTU.
    pub const FRAME_SAMPLES: usize = 480;
    pub const FRAME_BYTES: usize = FRAME_SAMPLES * 2;
    pub const MAX_AUDIO_PACKET: usize = HEADER_LEN + FRAME_BYTES;
}
