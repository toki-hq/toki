pub mod v1 {
    tonic::include_proto!("toki.v1");
}

/// Wire format constants shared by the client and the server's UDP audio relay.
///
/// Client → server packets:
///   [0..16]    16-byte session token (from `RegisterResponse.audio_token`)
///   [16]       1-byte packet kind (see `VERSION_*`)
///   [17..]     payload — for audio, little-endian i16 PCM samples
///
/// Server → other clients: just the PCM payload bytes. The server already
/// routed the packet, so peers don't need the header.
pub mod wire {
    pub const TOKEN_LEN: usize = 16;
    pub const HEADER_LEN: usize = TOKEN_LEN + 1;

    /// Empty packet used to register the client's UDP source address with the
    /// server (and keep NAT mappings alive). Not forwarded to peers.
    pub const VERSION_KEEPALIVE: u8 = 0;

    /// Raw PCM audio frame, little-endian i16, mono, 48 kHz.
    pub const VERSION_AUDIO_PCM: u8 = 1;

    pub const SAMPLE_RATE_HZ: u32 = 48_000;
    /// 10 ms of mono audio at 48 kHz — keeps each UDP packet well under MTU.
    pub const FRAME_SAMPLES: usize = 480;
    pub const FRAME_BYTES: usize = FRAME_SAMPLES * 2;
    pub const MAX_AUDIO_PACKET: usize = HEADER_LEN + FRAME_BYTES;
}
