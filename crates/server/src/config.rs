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
}

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
