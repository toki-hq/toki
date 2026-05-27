//! Server-side configuration loaded from a TOML file.
//!
//! Path resolution order:
//!   1. `$TOKI_CONFIG` if set — full path to the file.
//!   2. `./config.toml` next to the working directory, if it exists.
//!   3. Built-in defaults (open mode, no password).
//!
//! All read errors are non-fatal: a missing file silently falls back to
//! defaults so operators can launch a no-auth server with `cargo run`
//! and no extra files. A *malformed* file is fatal, though — we'd
//! rather refuse to boot than silently ignore a typo in the password
//! line and accidentally serve in open mode.
//!
//! Today the only configurable knob is the access password. Network
//! addresses still come from `TOKI_GRPC_ADDR` / `TOKI_AUDIO_ADDR` /
//! `TOKI_AUDIO_PUBLIC` environment variables so existing systemd unit
//! files and Docker compose files keep working without a config file.

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Top-level server config. Missing fields fall back to `Default`.
#[derive(Debug, Default, Deserialize)]
pub struct Config {
    /// Shared-secret password clients must echo back in their
    /// `RegisterRequest.password`. `None` (or an empty string after
    /// trimming) means the server runs in *open mode* and accepts any
    /// caller; otherwise the server compares the supplied value in
    /// constant time and rejects mismatches with `UNAUTHENTICATED`.
    ///
    /// Stored plaintext in the TOML file — we don't claim defense
    /// against an attacker who already has the config on disk; the
    /// password is a lightweight network gate, not a credential store.
    #[serde(default)]
    pub password: Option<String>,

    /// Optional TLS configuration for the gRPC signaling channel.
    /// When present, the server loads the named cert + key files
    /// (typically a real CA-issued cert like Let's Encrypt, or a
    /// self-signed pair from mkcert / step). When *absent*, the
    /// server auto-generates a self-signed cert via rcgen on first
    /// startup, persists it to `tls/{cert,key}.pem` next to the
    /// CWD, and reuses it on subsequent runs. Either way, gRPC is
    /// always TLS — there's no plaintext mode.
    #[serde(default)]
    pub tls: Option<TlsFiles>,
}

/// PEM-encoded certificate + private-key paths for the gRPC TLS
/// terminator. Either Let's Encrypt outputs (`fullchain.pem` +
/// `privkey.pem`) or self-signed pairs generated via mkcert / step.
#[derive(Debug, Deserialize)]
pub struct TlsFiles {
    pub cert: std::path::PathBuf,
    pub key: std::path::PathBuf,
}

/// Default location for the auto-generated self-signed cert pair.
/// Stored relative to CWD so a fresh checkout writes them under the
/// repo root by default; production deployments typically pass real
/// cert paths via the `[tls]` block in config.toml.
pub const DEFAULT_TLS_DIR: &str = "tls";
pub const DEFAULT_TLS_CERT: &str = "tls/cert.pem";
pub const DEFAULT_TLS_KEY: &str = "tls/key.pem";

impl Config {
    /// Resolve the config file's location and load it. Returns the
    /// loaded `Config` plus the path it actually came from (so the
    /// caller can log it). `None` for the path means "no file was
    /// resolved" — i.e. `$TOKI_CONFIG` was unset and there's no
    /// `./config.toml`, so we're returning hard-coded defaults.
    ///
    /// A file that exists but parses badly returns `Err` so the
    /// caller can refuse to boot — we'd rather fail loudly than
    /// silently disarm the password gate because of a TOML typo.
    pub fn load() -> anyhow::Result<(Self, Option<PathBuf>)> {
        let Some(path) = locate() else {
            return Ok((Self::default(), None));
        };
        let cfg = Self::from_path(&path)?;
        Ok((cfg, Some(path)))
    }

    pub fn from_path(path: &Path) -> anyhow::Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(s) => {
                let cfg: Config = toml::from_str(&s)
                    .map_err(|e| anyhow::anyhow!("parse {}: {}", path.display(), e))?;
                Ok(cfg)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(anyhow::anyhow!("read {}: {}", path.display(), e)),
        }
    }

    /// Normalised password: returns `Some` only when a non-empty value
    /// is configured. Trims whitespace so a TOML line like
    /// `password = " "` doesn't accidentally arm the gate.
    pub fn normalised_password(&self) -> Option<String> {
        match &self.password {
            Some(p) if !p.trim().is_empty() => Some(p.trim().to_string()),
            _ => None,
        }
    }
}

fn locate() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("TOKI_CONFIG") {
        return Some(PathBuf::from(p));
    }
    let default = PathBuf::from("config.toml");
    if default.exists() {
        return Some(default);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_runs_in_open_mode() {
        let cfg = Config::default();
        assert!(cfg.normalised_password().is_none());
        assert!(cfg.tls.is_none());
    }

    #[test]
    fn password_is_normalised() {
        let cfg: Config = toml::from_str("password = \"hunter2\"").unwrap();
        assert_eq!(cfg.normalised_password().as_deref(), Some("hunter2"));

        let cfg: Config = toml::from_str("password = \"  spaced  \"").unwrap();
        assert_eq!(cfg.normalised_password().as_deref(), Some("spaced"));
    }

    #[test]
    fn empty_or_whitespace_password_disarms_the_gate() {
        // Both "" and a whitespace-only value collapse to None so a
        // hand-edited config with `password = " "` doesn't
        // accidentally arm the gate against the operator's intent.
        let cfg: Config = toml::from_str("password = \"\"").unwrap();
        assert!(cfg.normalised_password().is_none());

        let cfg: Config = toml::from_str("password = \"   \"").unwrap();
        assert!(cfg.normalised_password().is_none());
    }

    #[test]
    fn tls_block_round_trips() {
        let raw = r#"
            [tls]
            cert = "/etc/toki/cert.pem"
            key = "/etc/toki/key.pem"
        "#;
        let cfg: Config = toml::from_str(raw).unwrap();
        let tls = cfg.tls.expect("expected [tls] block");
        assert_eq!(tls.cert.to_string_lossy(), "/etc/toki/cert.pem");
        assert_eq!(tls.key.to_string_lossy(), "/etc/toki/key.pem");
    }

    #[test]
    fn missing_file_returns_defaults() {
        // `from_path` must not error on missing file — that's the
        // common "no config" path and the server should boot.
        let cfg = Config::from_path(Path::new("/nonexistent/toki-test.toml")).unwrap();
        assert!(cfg.normalised_password().is_none());
        assert!(cfg.tls.is_none());
    }

    #[test]
    fn malformed_toml_is_fatal() {
        // Write a malformed TOML to a temp path, expect Err on read
        // — we want failures to surface loudly, not silently fall
        // back to defaults and (e.g.) disarm the password gate.
        let dir = std::env::temp_dir();
        let path = dir.join("toki-test-malformed.toml");
        std::fs::write(&path, "password = ").unwrap();
        let err = Config::from_path(&path).unwrap_err();
        let _ = std::fs::remove_file(&path);
        assert!(format!("{err:#}").contains("parse"));
    }
}
