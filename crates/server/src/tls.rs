//! Auto-generated self-signed TLS cert handling.
//!
//! gRPC is always TLS — there is no plaintext fallback. Two paths
//! land here:
//!
//!   * Operator supplied `[tls]` paths in `config.toml`. We read the
//!     PEM files from disk and hand them straight to Tonic. Use this
//!     for real CA-issued certs (Let's Encrypt, internal PKI).
//!
//!   * Operator supplied nothing. We try to load
//!     `tls/{cert,key}.pem` next to the CWD; if either is missing,
//!     we generate a fresh self-signed pair via `rcgen`, write both
//!     to disk with 0600 mode on the key, and use that. The pair
//!     persists across restarts so the cert fingerprint stays
//!     stable — useful both for caching on the client side and for
//!     "what cert am I serving?" log lines.
//!
//! Self-signed certs won't validate against the system trust store,
//! so the client installs a custom rustls verifier that accepts any
//! cert. Authentication of the *session* (caller has the right
//! password, audio packets are MAC'd with a key only the legitimate
//! client knows) is handled at the application layer.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::config::{TlsFiles, DEFAULT_TLS_CERT, DEFAULT_TLS_DIR, DEFAULT_TLS_KEY};

/// PEM-encoded certificate + private key pair ready to feed into
/// Tonic's `ServerTlsConfig::identity`. Always owned `Vec<u8>` so
/// the caller doesn't have to juggle borrows of the on-disk path.
#[derive(Debug)]
pub struct TlsMaterial {
    pub cert_pem: Vec<u8>,
    pub key_pem: Vec<u8>,
    /// Path the cert was loaded from (or written to on first run).
    /// Logged at startup so the operator knows where to find the
    /// fingerprint to pin into clients if they want hardened auth.
    pub source: PathBuf,
}

impl TlsMaterial {
    /// Resolve TLS material for the server's `[tls]` config. Either
    /// loads from the operator-specified paths or falls back to the
    /// auto-generated self-signed pair (creating it if absent).
    pub fn resolve(cfg: Option<&TlsFiles>) -> Result<Self> {
        match cfg {
            Some(files) => load_from_paths(&files.cert, &files.key),
            None => ensure_self_signed(),
        }
    }
}

fn load_from_paths(cert: &Path, key: &Path) -> Result<TlsMaterial> {
    let cert_pem =
        std::fs::read(cert).with_context(|| format!("read TLS cert {}", cert.display()))?;
    let key_pem = std::fs::read(key).with_context(|| format!("read TLS key {}", key.display()))?;
    Ok(TlsMaterial {
        cert_pem,
        key_pem,
        source: cert.to_path_buf(),
    })
}

/// Self-signed branch: load the existing pair from the default
/// location if both files are present; otherwise mint a fresh pair,
/// write it, and load it back. The "load it back" round-trip is
/// deliberate — it means rcgen and `std::fs` agree on the bytes
/// we're handing to Tonic, with no chance of a stale in-memory copy
/// drifting from disk.
fn ensure_self_signed() -> Result<TlsMaterial> {
    let cert_path = PathBuf::from(DEFAULT_TLS_CERT);
    let key_path = PathBuf::from(DEFAULT_TLS_KEY);
    if cert_path.exists() && key_path.exists() {
        return load_from_paths(&cert_path, &key_path);
    }
    generate_self_signed(&cert_path, &key_path)?;
    load_from_paths(&cert_path, &key_path)
}

fn generate_self_signed(cert_path: &Path, key_path: &Path) -> Result<()> {
    std::fs::create_dir_all(DEFAULT_TLS_DIR)
        .with_context(|| format!("create {DEFAULT_TLS_DIR}/"))?;

    // SANs: localhost + 127.0.0.1 cover the dev-loop case. Operators
    // running on a public hostname should provide a real cert via
    // the [tls] block anyway — this is the LAN-friendly default.
    let subject_alt_names = vec!["localhost".to_string(), "127.0.0.1".to_string()];
    let cert = rcgen::generate_simple_self_signed(subject_alt_names)
        .context("rcgen self-signed cert generation")?;

    std::fs::write(cert_path, cert.cert.pem().as_bytes())
        .with_context(|| format!("write {}", cert_path.display()))?;
    std::fs::write(key_path, cert.key_pair.serialize_pem().as_bytes())
        .with_context(|| format!("write {}", key_path.display()))?;
    tighten_key_perms(key_path);

    tracing::info!(
        cert = %cert_path.display(),
        key = %key_path.display(),
        "generated self-signed TLS cert (no [tls] block in config)"
    );
    Ok(())
}

#[cfg(unix)]
fn tighten_key_perms(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
        tracing::warn!(error = %e, path = %path.display(), "could not chmod TLS key");
    }
}

#[cfg(not(unix))]
fn tighten_key_perms(_path: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: build a `TlsFiles` referencing a freshly-written
    /// rcgen cert pair in a temp directory. Returns the dir path so
    /// the caller can clean up.
    fn write_temp_cert() -> (PathBuf, TlsFiles) {
        let dir = std::env::temp_dir().join(format!("toki-tls-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cert_path = dir.join("cert.pem");
        let key_path = dir.join("key.pem");
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        std::fs::write(&cert_path, cert.cert.pem()).unwrap();
        std::fs::write(&key_path, cert.key_pair.serialize_pem()).unwrap();
        (
            dir,
            TlsFiles {
                cert: cert_path,
                key: key_path,
            },
        )
    }

    #[test]
    fn resolve_loads_operator_supplied_paths() {
        let (dir, files) = write_temp_cert();
        let material = TlsMaterial::resolve(Some(&files)).unwrap();
        assert_eq!(material.source, files.cert);
        // PEM output starts with `-----BEGIN`.
        let cert_text = std::str::from_utf8(&material.cert_pem).unwrap();
        assert!(cert_text.starts_with("-----BEGIN"));
        let key_text = std::str::from_utf8(&material.key_pem).unwrap();
        assert!(key_text.starts_with("-----BEGIN"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_errors_on_missing_operator_path() {
        let files = TlsFiles {
            cert: PathBuf::from("/nonexistent/cert.pem"),
            key: PathBuf::from("/nonexistent/key.pem"),
        };
        let err = TlsMaterial::resolve(Some(&files)).unwrap_err();
        // The anyhow chain mentions the path so an operator can
        // debug the config without grepping the source.
        assert!(format!("{err:#}").contains("/nonexistent"));
    }

    /// Auto-generation walks the same code path the server boots
    /// through. We isolate it under a temp CWD because the resolver
    /// uses relative `tls/` paths from `cd`.
    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn auto_generates_self_signed_pair_when_no_config() {
        let dir = std::env::temp_dir().join(format!("toki-tls-auto-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let original_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(&dir).unwrap();

        // First call: no files yet, generator runs.
        let first = TlsMaterial::resolve(None).unwrap();
        assert!(dir.join("tls").join("cert.pem").exists());
        assert!(dir.join("tls").join("key.pem").exists());

        // Key file must be 0600 — that's the whole point of
        // `tighten_key_perms`.
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dir.join("tls").join("key.pem"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "key file must be chmod 0600, was {mode:o}");

        // Second call: files already exist, generator does NOT run
        // and the cert bytes match the first call — i.e. the same
        // identity persists across restarts.
        let second = TlsMaterial::resolve(None).unwrap();
        assert_eq!(first.cert_pem, second.cert_pem);

        std::env::set_current_dir(original_cwd).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
