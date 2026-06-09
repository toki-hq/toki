//! Self-generated, keypair-backed client identity.
//!
//! On first use the client generates an **ed25519 keypair** and persists
//! it to `identity.toml` next to the regular config. The public key *is*
//! the identity: at register the client proves possession of the private
//! key by signing a server-issued challenge (see
//! `Signaling.IdentityChallenge`), so the identity string shown in admin
//! panels and audit logs cannot be claimed by an observer.
//!
//! Alongside the key the file records:
//!   * `first_callsign` — the display name in use at generation time,
//!     normalized; the human-readable prefix of the display id
//!     (`COTON-7Q4XF9KB`). Fixed forever — later renames don't touch it.
//!   * `origin_client_id` — the first session id any server ever
//!     assigned this identity. A claimed provenance breadcrumb,
//!     recorded once after the first identity-ful register.
//!
//! The **machine hash** ([`machine_hash`]) is deliberately *not* stored
//! in this file: it's derived fresh from the OS machine id on every
//! register, so wiping the config (new keypair → new identity) still
//! presents the same machine hash — the server-side correlation anchor
//! against ban evasion. The raw machine id never leaves this process;
//! only the salted BLAKE3 hash goes on the wire (see
//! `toki_proto::identity::machine_hash`).
//!
//! Everything here is best-effort: a missing config dir, an unreadable
//! file, or an exotic platform without a machine id degrades to
//! "register identity-less", never to a failed connect.

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use ed25519_dalek::{Signer, SigningKey};
use serde::{Deserialize, Serialize};

use toki_proto::identity as id;

const FILENAME: &str = "identity.toml";

/// On-disk shape of `identity.toml`. The secret key is hex — the file
/// is chmod 0600 like the main config (it holds the server password
/// there; the identity seed here is exactly as sensitive).
#[derive(Serialize, Deserialize)]
struct IdentityFile {
    /// Hex-encoded 32-byte ed25519 seed. Leaking this leaks the
    /// identity; deleting it mints a new one.
    secret_key: String,
    /// Unix seconds at generation. Informational.
    created_at: u64,
    /// Normalized callsign captured at generation (display-id prefix).
    first_callsign: String,
    /// First server-assigned session id this identity ever received.
    /// Empty until the first successful identity-ful register.
    #[serde(default)]
    origin_client_id: String,
}

/// A loaded (or freshly minted) identity, plus where to persist updates.
pub struct Identity {
    signing: SigningKey,
    pub created_at: u64,
    pub first_callsign: String,
    pub origin_client_id: String,
    /// `None` when no config dir could be resolved — the identity then
    /// lives for this process only (and a fresh one is minted next run;
    /// nothing better is possible without a place to persist).
    path: Option<PathBuf>,
}

impl Identity {
    /// Load the persisted identity, or generate + persist a new one.
    /// `display_name` seeds `first_callsign` only on generation.
    ///
    /// A corrupt or unreadable file is treated as absent (a fresh
    /// identity is minted over it) — the alternative, refusing to
    /// connect, would brick the client over a damaged optional file.
    pub fn load_or_generate(display_name: &str) -> Self {
        Self::load_or_generate_at(default_path(), display_name)
    }

    /// Path-injectable core of [`Self::load_or_generate`] (tests point
    /// it at a temp dir).
    fn load_or_generate_at(path: Option<PathBuf>, display_name: &str) -> Self {
        if let Some(p) = &path {
            match fs::read_to_string(p) {
                Ok(s) => match toml::from_str::<IdentityFile>(&s)
                    .ok()
                    .and_then(|f| Some((SigningKey::from_bytes(&hex_to_seed(&f.secret_key)?), f)))
                {
                    Some((signing, f)) => {
                        return Self {
                            signing,
                            created_at: f.created_at,
                            first_callsign: f.first_callsign,
                            origin_client_id: f.origin_client_id,
                            path,
                        }
                    }
                    None => {
                        tracing::warn!(path = %p.display(), "identity file corrupt; generating a fresh identity");
                    }
                },
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    tracing::warn!(error = %e, path = %p.display(), "could not read identity file; generating a fresh identity");
                }
            }
        }

        // Mint a new identity. getrandom failing means the OS entropy
        // source is broken — at that point rustls couldn't handshake
        // either, so an expect is honest rather than alarmist.
        let mut seed = [0u8; 32];
        getrandom::getrandom(&mut seed).expect("OS entropy source unavailable");
        let signing = SigningKey::from_bytes(&seed);
        let identity = Self {
            signing,
            created_at: now_unix(),
            first_callsign: id::normalize_callsign(display_name),
            origin_client_id: String::new(),
            path,
        };
        tracing::info!(identity = %identity.display_id(), "generated new client identity");
        identity.persist();
        identity
    }

    /// The 32-byte ed25519 public key — the canonical identity.
    pub fn pubkey_bytes(&self) -> [u8; 32] {
        self.signing.verifying_key().to_bytes()
    }

    /// Human-readable identity string, e.g. `COTON-7Q4XF9KB`.
    pub fn display_id(&self) -> String {
        id::display_id(&self.first_callsign, &self.pubkey_bytes())
    }

    /// Sign a register challenge: ed25519 over the domain-separated
    /// payload `"toki-register-v1" || nonce`. 64 bytes.
    pub fn sign_challenge(&self, nonce: &[u8]) -> Vec<u8> {
        self.signing.sign(&id::signing_payload(nonce)).to_vec()
    }

    /// Record the first session id this identity was ever assigned —
    /// once. Subsequent calls are no-ops, so every register can call
    /// this unconditionally after an identity-ful handshake.
    pub fn record_origin(&mut self, client_id: &str) {
        if !self.origin_client_id.is_empty() || client_id.is_empty() {
            return;
        }
        self.origin_client_id = client_id.to_string();
        self.persist();
    }

    /// Best-effort write-back of the current state. Failures log and
    /// move on (worst case: the identity regenerates next run, or an
    /// origin id goes unrecorded).
    fn persist(&self) {
        let Some(path) = &self.path else { return };
        if let Some(parent) = path.parent() {
            if let Err(e) = fs::create_dir_all(parent) {
                tracing::warn!(error = %e, path = %parent.display(), "could not create identity dir");
                return;
            }
        }
        let file = IdentityFile {
            secret_key: seed_to_hex(&self.signing.to_bytes()),
            created_at: self.created_at,
            first_callsign: self.first_callsign.clone(),
            origin_client_id: self.origin_client_id.clone(),
        };
        match toml::to_string_pretty(&file) {
            Ok(s) => {
                if let Err(e) = fs::write(path, s) {
                    tracing::warn!(error = %e, path = %path.display(), "could not write identity file");
                    return;
                }
                tighten_permissions(path);
            }
            Err(e) => tracing::warn!(error = %e, "could not serialize identity file"),
        }
    }
}

/// `identity.toml` in the same per-user config dir as `config.toml`.
fn default_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("toki").join(FILENAME))
}

/// Salted machine-fingerprint hash to present at register, or `None`
/// when the platform exposes no machine id (the register then simply
/// omits the attribute). Derived fresh each call — never persisted —
/// see the module docs for why.
pub fn machine_hash() -> Option<String> {
    machine_id().map(|raw| id::machine_hash(&raw))
}

/// The raw OS machine identifier. Read per-platform with no extra
/// dependencies (file read on Linux, one short-lived system tool on
/// macOS/Windows). The value stays in-process; only its salted hash
/// ever goes on the wire.
fn machine_id() -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        // systemd's machine id, with the pre-systemd dbus fallback.
        fs::read_to_string("/etc/machine-id")
            .or_else(|_| fs::read_to_string("/var/lib/dbus/machine-id"))
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }
    #[cfg(target_os = "macos")]
    {
        // IOPlatformUUID from the IOKit registry. `ioreg` ships with
        // the OS; parsing its one relevant line beats linking IOKit.
        let out = Command::new("ioreg")
            .args(["-rd1", "-c", "IOPlatformExpertDevice"])
            .output()
            .ok()?;
        parse_quoted_value(&String::from_utf8_lossy(&out.stdout), "IOPlatformUUID")
    }
    #[cfg(target_os = "windows")]
    {
        // MachineGuid from the registry via reg.exe (ships with the OS).
        let out = Command::new("reg")
            .args([
                "query",
                r"HKLM\SOFTWARE\Microsoft\Cryptography",
                "/v",
                "MachineGuid",
            ])
            .output()
            .ok()?;
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .find(|l| l.contains("MachineGuid"))
            .and_then(|l| l.split_whitespace().last())
            .map(str::to_string)
            .filter(|s| !s.is_empty())
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        None
    }
}

/// Extract `"VALUE"` from a `ioreg`-style line `"KEY" = "VALUE"`.
#[cfg(target_os = "macos")]
fn parse_quoted_value(haystack: &str, key: &str) -> Option<String> {
    let line = haystack.lines().find(|l| l.contains(key))?;
    let (_, after_eq) = line.split_once('=')?;
    let value = after_eq.split('"').nth(1)?;
    (!value.is_empty()).then(|| value.to_string())
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn seed_to_hex(seed: &[u8; 32]) -> String {
    seed.iter().map(|b| format!("{b:02x}")).collect()
}

/// Strict hex → 32-byte seed; `None` on any length/character problem
/// (the caller treats that as a corrupt file).
fn hex_to_seed(hex: &str) -> Option<[u8; 32]> {
    if hex.len() != 64 {
        return None;
    }
    let mut seed = [0u8; 32];
    for (i, byte) in seed.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(seed)
}

/// Same posture as the main config: the seed must not be readable by
/// other local users. No-op on Windows (profile ACLs already scope it).
#[cfg(unix)]
fn tighten_permissions(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Err(e) = fs::set_permissions(path, fs::Permissions::from_mode(0o600)) {
        tracing::warn!(error = %e, path = %path.display(), "could not tighten identity file permissions");
    }
}

#[cfg(not(unix))]
fn tighten_permissions(_path: &std::path::Path) {}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Verifier, VerifyingKey};

    fn temp_identity_path(tag: &str) -> PathBuf {
        std::env::temp_dir()
            .join(format!("toki-identity-test-{}-{tag}", std::process::id()))
            .join(FILENAME)
    }

    #[test]
    fn generate_then_reload_round_trips() {
        let path = temp_identity_path("roundtrip");
        let _ = fs::remove_dir_all(path.parent().unwrap());

        let fresh = Identity::load_or_generate_at(Some(path.clone()), "coton");
        assert_eq!(fresh.first_callsign, "COTON");
        assert!(fresh.origin_client_id.is_empty());

        let reloaded = Identity::load_or_generate_at(Some(path.clone()), "OTHER-NAME");
        // Same key, and first_callsign stays the generation-time one.
        assert_eq!(reloaded.pubkey_bytes(), fresh.pubkey_bytes());
        assert_eq!(reloaded.first_callsign, "COTON");
        assert_eq!(reloaded.display_id(), fresh.display_id());

        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn corrupt_file_regenerates_instead_of_failing() {
        let path = temp_identity_path("corrupt");
        let _ = fs::remove_dir_all(path.parent().unwrap());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "secret_key = \"not hex at all\"").unwrap();

        let identity = Identity::load_or_generate_at(Some(path.clone()), "anyone");
        assert_eq!(identity.pubkey_bytes().len(), 32);
        // And the rewrite is loadable again.
        let reloaded = Identity::load_or_generate_at(Some(path.clone()), "x");
        assert_eq!(reloaded.pubkey_bytes(), identity.pubkey_bytes());

        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn challenge_signature_verifies_against_pubkey() {
        let identity = Identity::load_or_generate_at(None, "coton");
        let nonce = b"server-issued-nonce";
        let sig_bytes = identity.sign_challenge(nonce);
        assert_eq!(sig_bytes.len(), toki_proto::identity::SIGNATURE_LEN);

        // Verify exactly as the server will: pubkey + domain-separated payload.
        let pubkey = VerifyingKey::from_bytes(&identity.pubkey_bytes()).unwrap();
        let sig = ed25519_dalek::Signature::from_slice(&sig_bytes).unwrap();
        let payload = toki_proto::identity::signing_payload(nonce);
        assert!(pubkey.verify(&payload, &sig).is_ok());
        // And a tampered nonce fails.
        let tampered = toki_proto::identity::signing_payload(b"other-nonce");
        assert!(pubkey.verify(&tampered, &sig).is_err());
    }

    #[test]
    fn record_origin_writes_once() {
        let path = temp_identity_path("origin");
        let _ = fs::remove_dir_all(path.parent().unwrap());

        let mut identity = Identity::load_or_generate_at(Some(path.clone()), "coton");
        identity.record_origin("session-1");
        identity.record_origin("session-2"); // ignored — already recorded

        let reloaded = Identity::load_or_generate_at(Some(path.clone()), "x");
        assert_eq!(reloaded.origin_client_id, "session-1");

        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn identity_file_is_chmod_0600() {
        use std::os::unix::fs::PermissionsExt;
        let path = temp_identity_path("perms");
        let _ = fs::remove_dir_all(path.parent().unwrap());

        let _identity = Identity::load_or_generate_at(Some(path.clone()), "coton");
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "identity seed must be owner-only");

        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn hex_seed_round_trips_and_rejects_garbage() {
        let seed = [0xA5u8; 32];
        assert_eq!(hex_to_seed(&seed_to_hex(&seed)), Some(seed));
        assert_eq!(hex_to_seed("short"), None);
        assert_eq!(hex_to_seed(&"zz".repeat(32)), None);
    }
}
