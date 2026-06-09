//! Server side of the client-identity register handshake.
//!
//! Two halves:
//!
//! 1. **Stateless challenge nonces** ([`issue`] / [`verify_nonce`]).
//!    The `IdentityChallenge` RPC hands the client an opaque nonce to
//!    sign; `Register` must echo it back. The nonce embeds its own
//!    issue timestamp and a BLAKE3-keyed tag over a per-boot secret,
//!    so the server keeps **no nonce table** — verification is a tag
//!    check + a TTL window. A nonce can technically be replayed
//!    within its ~60 s window, which is harmless: replaying it only
//!    ever proves possession of the *same* private key to the *same*
//!    server, minting another session for the same identity — exactly
//!    what a fresh challenge would do.
//!
//! 2. **Identity verification** ([`verify_register`]): parse the
//!    pubkey, check the nonce, verify the ed25519 signature over the
//!    domain-separated payload (see `toki_proto::identity`), and
//!    sanity-check the claimed attributes. Failures are
//!    `UNAUTHENTICATED` — a present-but-invalid identity is rejected,
//!    never silently downgraded to an anonymous session (the proto
//!    contract; it keeps impersonation attempts from landing as
//!    "connected, just unverified").
//!
//! The challenge key is generated fresh at process start. Challenges
//! don't need to survive a restart: a client whose connect spans one
//! simply fails the register and reconnects with a fresh nonce.

use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use tonic::Status;

use toki_proto::identity as id;
use toki_proto::v1::RegisterRequest;

use crate::state::ClientIdentity;

/// Random prefix of a nonce — makes every challenge unique.
const NONCE_RANDOM_LEN: usize = 16;
/// Little-endian unix-seconds issue timestamp.
const NONCE_TS_LEN: usize = 8;
/// Full BLAKE3 keyed tag (kept at 32 bytes so verification can use
/// `blake3::Hash`'s constant-time equality).
const NONCE_TAG_LEN: usize = 32;
/// Total nonce length on the wire.
pub const NONCE_LEN: usize = NONCE_RANDOM_LEN + NONCE_TS_LEN + NONCE_TAG_LEN;

/// How long an issued challenge stays valid. Long enough for a slow
/// TLS handshake + register round-trip, short enough that a captured
/// nonce is useless almost immediately.
const CHALLENGE_TTL_SECS: u64 = 60;
/// Tolerated clock skew for a nonce that appears to be from the
/// future (e.g. the host clock stepped backwards between issue and
/// verify). Anything beyond this is treated as forged.
const FUTURE_SKEW_SECS: u64 = 5;

/// Per-boot secret keying the challenge tags. Deliberately **not**
/// persisted: restarting the server invalidates outstanding
/// challenges, nothing else.
pub struct ChallengeKey([u8; 32]);

impl ChallengeKey {
    /// Fresh random key. A failed entropy read means the host CSPRNG
    /// is broken — TLS couldn't come up either, so panicking here is
    /// honest fail-fast, not drama.
    pub fn generate() -> Self {
        let mut key = [0u8; 32];
        getrandom::getrandom(&mut key).expect("OS entropy source unavailable");
        Self(key)
    }

    fn tag(&self, random_and_ts: &[u8]) -> blake3::Hash {
        blake3::keyed_hash(&self.0, random_and_ts)
    }
}

/// Build a challenge nonce: `random(16) || unix_secs(8 LE) || tag(32)`
/// where `tag = BLAKE3_keyed(key, random || ts)`.
pub fn issue(key: &ChallengeKey, now_unix: u64) -> Vec<u8> {
    let mut nonce = Vec::with_capacity(NONCE_LEN);
    let mut random = [0u8; NONCE_RANDOM_LEN];
    getrandom::getrandom(&mut random).expect("OS entropy source unavailable");
    nonce.extend_from_slice(&random);
    nonce.extend_from_slice(&now_unix.to_le_bytes());
    let tag = key.tag(&nonce);
    nonce.extend_from_slice(tag.as_bytes());
    nonce
}

/// Check a nonce came from [`issue`] with this key and is inside its
/// TTL window. Error strings are caller-facing reasons (the register
/// handler folds them into its UNAUTHENTICATED message).
pub fn verify_nonce(key: &ChallengeKey, nonce: &[u8], now_unix: u64) -> Result<(), &'static str> {
    if nonce.len() != NONCE_LEN {
        return Err("malformed challenge nonce");
    }
    let (body, tag_bytes) = nonce.split_at(NONCE_RANDOM_LEN + NONCE_TS_LEN);
    // blake3::Hash equality is constant-time — that's the whole reason
    // the tag stays at the full 32 bytes.
    let expected = blake3::Hash::from(<[u8; NONCE_TAG_LEN]>::try_from(tag_bytes).unwrap());
    if key.tag(body) != expected {
        return Err("challenge nonce not issued by this server");
    }
    let ts = u64::from_le_bytes(body[NONCE_RANDOM_LEN..].try_into().unwrap());
    if ts > now_unix + FUTURE_SKEW_SECS {
        return Err("challenge nonce from the future");
    }
    if now_unix.saturating_sub(ts) > CHALLENGE_TTL_SECS {
        return Err("challenge expired; fetch a new one and retry");
    }
    Ok(())
}

/// The verified outcome of a register's identity fields, minus the
/// bookkeeping the caller fills from the shared identity map
/// (`first_seen` comes from the prior record, when one exists).
#[derive(Debug)]
pub struct VerifiedIdentity {
    pub pubkey_hex: String,
    /// Lowercased 64-hex machine hash, or empty when the client had
    /// no machine id to hash.
    pub machine_hash: String,
    /// Claimed provenance id, length-capped. Empty when unset.
    pub origin_client_id: String,
}

/// Verify the identity fields of a register request. Returns:
///   * `Ok(None)` — no identity presented (empty pubkey, the
///     pre-identity / opted-out path). Stray crypto fields without a
///     pubkey are rejected rather than ignored.
///   * `Ok(Some(_))` — possession of the private key proven against a
///     live challenge; attributes sanitized.
///   * `Err(UNAUTHENTICATED)` — present but invalid. Never downgraded.
///
/// (`tonic::Status` is a fat error type; the same allow already covers
/// the validation helpers that return it.)
#[allow(clippy::result_large_err)]
pub fn verify_register(
    key: &ChallengeKey,
    req: &RegisterRequest,
    now_unix: u64,
) -> Result<Option<VerifiedIdentity>, Status> {
    if req.identity_pubkey.is_empty() {
        if !req.challenge_nonce.is_empty() || !req.identity_signature.is_empty() {
            return Err(Status::unauthenticated(
                "identity signature without a public key",
            ));
        }
        return Ok(None);
    }

    let pubkey_bytes: [u8; id::PUBKEY_LEN] = req
        .identity_pubkey
        .as_slice()
        .try_into()
        .map_err(|_| Status::unauthenticated("malformed identity public key"))?;
    let pubkey = VerifyingKey::from_bytes(&pubkey_bytes)
        .map_err(|_| Status::unauthenticated("malformed identity public key"))?;

    verify_nonce(key, &req.challenge_nonce, now_unix)
        .map_err(|reason| Status::unauthenticated(format!("identity challenge: {reason}")))?;

    let signature = Signature::from_slice(&req.identity_signature)
        .map_err(|_| Status::unauthenticated("malformed identity signature"))?;
    let payload = id::signing_payload(&req.challenge_nonce);
    pubkey
        .verify(&payload, &signature)
        .map_err(|_| Status::unauthenticated("identity signature verification failed"))?;

    // Attributes are claimed, not proven — but malformed ones are an
    // implementation bug or a probe, so they fail the register too
    // rather than landing as junk in the admin DB.
    let machine_hash = req.machine_hash.to_ascii_lowercase();
    if !machine_hash.is_empty()
        && (machine_hash.len() != 64 || !machine_hash.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return Err(Status::unauthenticated("malformed machine hash"));
    }
    let origin_client_id = req.origin_client_id.trim().to_string();
    if origin_client_id.len() > 64
        || origin_client_id
            .bytes()
            .any(|b| !b.is_ascii_graphic() && b != b' ')
    {
        return Err(Status::unauthenticated("malformed origin client id"));
    }

    Ok(Some(VerifiedIdentity {
        pubkey_hex: hex(&pubkey_bytes),
        machine_hash,
        origin_client_id,
    }))
}

/// Build the registry-facing identity for a verified register. The
/// display id is purely key-derived (the 8-char fingerprint), so the
/// only merge against the prior record is `first_seen`.
pub fn merged_identity(
    verified: &VerifiedIdentity,
    prior: Option<&crate::state::IdentityRecord>,
    now_unix: i64,
) -> ClientIdentity {
    let pubkey_bytes = unhex32(&verified.pubkey_hex);
    ClientIdentity {
        display_id: id::fingerprint(&pubkey_bytes),
        pubkey_hex: verified.pubkey_hex.clone(),
        machine_hash: verified.machine_hash.clone(),
        first_seen: prior.map(|r| r.first_seen).unwrap_or(now_unix),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Inverse of [`hex`] for the 32-byte pubkey we ourselves encoded —
/// infallible by construction (the hex came from `verify_register`).
fn unhex32(hex: &str) -> [u8; 32] {
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap_or(0);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    fn test_request(signing: &SigningKey, nonce: Vec<u8>) -> RegisterRequest {
        let signature = signing.sign(&id::signing_payload(&nonce)).to_vec();
        RegisterRequest {
            display_name: "anon".into(),
            identity_pubkey: signing.verifying_key().to_bytes().to_vec(),
            challenge_nonce: nonce,
            identity_signature: signature,
            ..Default::default()
        }
    }

    #[test]
    fn nonce_round_trips_inside_ttl() {
        let key = ChallengeKey::generate();
        let nonce = issue(&key, 1_000_000);
        assert_eq!(nonce.len(), NONCE_LEN);
        assert!(verify_nonce(&key, &nonce, 1_000_000).is_ok());
        assert!(verify_nonce(&key, &nonce, 1_000_000 + CHALLENGE_TTL_SECS).is_ok());
    }

    #[test]
    fn nonce_rejects_expiry_tamper_and_foreign_key() {
        let key = ChallengeKey::generate();
        let nonce = issue(&key, 1_000_000);
        // Expired.
        assert!(verify_nonce(&key, &nonce, 1_000_000 + CHALLENGE_TTL_SECS + 1).is_err());
        // From the future (clock stepped back past the skew window).
        assert!(verify_nonce(&key, &nonce, 1_000_000 - FUTURE_SKEW_SECS - 1).is_err());
        // Tampered byte (flip one bit in the timestamp region).
        let mut tampered = nonce.clone();
        tampered[NONCE_RANDOM_LEN] ^= 1;
        assert!(verify_nonce(&key, &tampered, 1_000_000).is_err());
        // Issued by a different server boot.
        let other = ChallengeKey::generate();
        assert!(verify_nonce(&other, &nonce, 1_000_000).is_err());
        // Garbage length.
        assert!(verify_nonce(&key, b"short", 1_000_000).is_err());
    }

    #[test]
    fn register_without_identity_is_ok_none() {
        let key = ChallengeKey::generate();
        let req = RegisterRequest {
            display_name: "anon".into(),
            ..Default::default()
        };
        assert!(verify_register(&key, &req, 1_000_000).unwrap().is_none());
    }

    #[test]
    fn register_with_valid_identity_verifies() {
        let key = ChallengeKey::generate();
        let signing = SigningKey::from_bytes(&[7u8; 32]);
        let req = test_request(&signing, issue(&key, 1_000_000));
        let verified = verify_register(&key, &req, 1_000_000).unwrap().unwrap();
        assert_eq!(verified.pubkey_hex.len(), 64);
        assert!(verified.machine_hash.is_empty());
    }

    #[test]
    fn register_rejects_bad_signature_and_stale_nonce() {
        let key = ChallengeKey::generate();
        let signing = SigningKey::from_bytes(&[7u8; 32]);

        // Signature by a different key over the same nonce.
        let nonce = issue(&key, 1_000_000);
        let mut req = test_request(&signing, nonce.clone());
        let impostor = SigningKey::from_bytes(&[8u8; 32]);
        req.identity_signature = impostor.sign(&id::signing_payload(&nonce)).to_vec();
        let err = verify_register(&key, &req, 1_000_000).unwrap_err();
        assert_eq!(err.code(), tonic::Code::Unauthenticated);

        // Stale nonce.
        let req = test_request(&signing, issue(&key, 1_000_000));
        let err = verify_register(&key, &req, 1_000_000 + CHALLENGE_TTL_SECS + 1).unwrap_err();
        assert_eq!(err.code(), tonic::Code::Unauthenticated);

        // Crypto fields without a pubkey.
        let req = RegisterRequest {
            display_name: "anon".into(),
            challenge_nonce: issue(&key, 1_000_000),
            ..Default::default()
        };
        let err = verify_register(&key, &req, 1_000_000).unwrap_err();
        assert_eq!(err.code(), tonic::Code::Unauthenticated);
    }

    #[test]
    fn register_rejects_malformed_attributes() {
        let key = ChallengeKey::generate();
        let signing = SigningKey::from_bytes(&[7u8; 32]);

        let mut req = test_request(&signing, issue(&key, 1_000_000));
        req.machine_hash = "not-hex".into();
        assert!(verify_register(&key, &req, 1_000_000).is_err());

        let mut req = test_request(&signing, issue(&key, 1_000_000));
        req.machine_hash = "AB".repeat(32); // uppercase hex → accepted, lowercased
        let verified = verify_register(&key, &req, 1_000_000).unwrap().unwrap();
        assert_eq!(verified.machine_hash, "ab".repeat(32));

        let mut req = test_request(&signing, issue(&key, 1_000_000));
        req.origin_client_id = "x".repeat(65);
        assert!(verify_register(&key, &req, 1_000_000).is_err());
    }

    #[test]
    fn merged_identity_derives_display_id_and_pins_first_seen() {
        let key = ChallengeKey::generate();
        let signing = SigningKey::from_bytes(&[7u8; 32]);
        let req = test_request(&signing, issue(&key, 1_000_000));
        let verified = verify_register(&key, &req, 1_000_000).unwrap().unwrap();

        // No prior record → first_seen = now; display id is the bare
        // key fingerprint.
        let fresh = merged_identity(&verified, None, 42);
        assert_eq!(
            fresh.display_id,
            id::fingerprint(&signing.verifying_key().to_bytes())
        );
        assert_eq!(fresh.first_seen, 42);

        // Prior record → its first_seen wins; the display id is the
        // same either way (purely key-derived).
        let prior = crate::state::IdentityRecord {
            display_id: String::new(), // derived, not read back
            last_callsign: "anon".into(),
            machine_hash: String::new(),
            origin_client_id: String::new(),
            first_seen: 7,
            last_seen: 7,
            last_ip: String::new(),
        };
        let merged = merged_identity(&verified, Some(&prior), 42);
        assert_eq!(merged.display_id, fresh.display_id);
        assert_eq!(merged.first_seen, 7);
    }
}
