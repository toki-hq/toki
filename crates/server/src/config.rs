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
//! Network addresses for the gRPC + UDP audio listeners come from
//! `TOKI_GRPC_ADDR` / `TOKI_AUDIO_ADDR` / `TOKI_AUDIO_PUBLIC` env
//! variables so existing systemd unit files and Docker compose files
//! keep working without a config file. The admin dashboard is
//! always-on; its bind / port / db path live in the `[admin]` block
//! with defaults that fire when the block is absent.

use std::path::{Path, PathBuf};

use anyhow::Context as _;
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

    /// Admin web panel settings. The dashboard is always exposed —
    /// the `[admin]` block exists only to override the defaults
    /// (`bind = "127.0.0.1"`, `port = 8000`, etc). On first boot,
    /// if the sqlite store at `db_path` has no admin users, one is
    /// seeded with a random password and logged once at WARN level.
    /// Operators who don't want the panel reachable from the LAN
    /// keep the default `bind = "127.0.0.1"` and rely on the
    /// loopback to keep it private.
    #[serde(default)]
    pub admin: AdminConfig,

    /// Automatic TLS via Let's Encrypt (ACME HTTP-01). Disabled by
    /// default. When enabled with a domain + agreed ToS, the server
    /// obtains a browser-trusted cert on first boot and renews it in
    /// the background, hot-swapping it into both the gRPC and admin
    /// listeners with no restart. A parallel cert *source* to `[tls]`;
    /// precedence is explicit `[tls]` paths > `[acme]` > self-signed.
    #[serde(default)]
    pub acme: AcmeConfig,
}

/// Automatic-certificate (ACME / Let's Encrypt) settings. All fields
/// default to "off", so an absent `[acme]` block leaves the server on
/// its self-signed / operator-`[tls]` path exactly as before.
///
/// HTTP-01 has hard external requirements the operator must satisfy:
/// a public DNS name resolving to this host, and inbound **port 80**
/// reachable from the internet for issuance *and* every renewal.
#[derive(Debug, Clone, Deserialize)]
pub struct AcmeConfig {
    /// Master switch. Even with domains set, ACME stays dormant unless
    /// this is `true` — avoids surprise external calls / port-80 binds.
    #[serde(default)]
    pub enabled: bool,
    /// Domain(s) the cert is issued for. The first is the primary; any
    /// extras become SANs. Bare IPs are invalid for Let's Encrypt.
    #[serde(default)]
    pub domains: Vec<String>,
    /// Contact email registered with the ACME account (expiry notices).
    #[serde(default)]
    pub contact_email: String,
    /// Must be `true` to proceed — the operator agrees to the ACME
    /// provider's Terms of Service (Let's Encrypt requires this). We
    /// refuse to boot with ACME enabled but ToS not agreed.
    #[serde(default)]
    pub terms_of_service_agreed: bool,
    /// Use Let's Encrypt's *staging* directory (untrusted roots, far
    /// higher rate limits) while testing the setup. Flip to `false`
    /// for real, browser-trusted certs once issuance works end-to-end.
    #[serde(default)]
    pub staging: bool,
    /// Bind address for the port-80 listener that serves the HTTP-01
    /// challenge and 308-redirects everything else to HTTPS. Must be
    /// publicly reachable on port 80; `0.0.0.0:80` by default.
    #[serde(default = "default_acme_http_bind")]
    pub http_bind: String,
}

impl Default for AcmeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            domains: Vec::new(),
            contact_email: String::new(),
            terms_of_service_agreed: false,
            staging: false,
            http_bind: default_acme_http_bind(),
        }
    }
}

fn default_acme_http_bind() -> String {
    "0.0.0.0:80".to_string()
}

/// Env-var overrides for `[acme]`, mirroring the `[admin]` pattern.
pub const ENV_ACME_ENABLED: &str = "TOKI_ACME_ENABLED";
pub const ENV_ACME_DOMAINS: &str = "TOKI_ACME_DOMAINS";
pub const ENV_ACME_EMAIL: &str = "TOKI_ACME_EMAIL";
pub const ENV_ACME_STAGING: &str = "TOKI_ACME_STAGING";

/// Subdirectory (under the data dir) for ACME state: account
/// credentials + the issued cert/key + the `issued_at` marker.
pub const DEFAULT_ACME_DIR: &str = "tls/acme";

impl AcmeConfig {
    /// Whether ACME issuance should actually run: explicitly enabled
    /// and at least one domain configured.
    pub fn is_active(&self) -> bool {
        self.enabled && !self.domains.is_empty()
    }

    /// Validate an active ACME config. Surfaces operator mistakes as a
    /// clear boot failure rather than a confusing ACME error later.
    pub fn validate(&self) -> anyhow::Result<()> {
        if !self.is_active() {
            return Ok(());
        }
        if self.contact_email.trim().is_empty() {
            anyhow::bail!("[acme] enabled but contact_email is empty");
        }
        if !self.terms_of_service_agreed {
            anyhow::bail!(
                "[acme] enabled but terms_of_service_agreed = false; \
                 set it to true to accept the ACME provider's Terms of Service"
            );
        }
        Ok(())
    }

    /// Apply `TOKI_ACME_*` env vars over the TOML values (env > TOML >
    /// defaults). `TOKI_ACME_DOMAINS` is comma-separated.
    pub fn apply_env_overrides(&mut self) -> anyhow::Result<()> {
        if let Ok(v) = std::env::var(ENV_ACME_ENABLED) {
            self.enabled = matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes");
        }
        if let Ok(v) = std::env::var(ENV_ACME_DOMAINS) {
            self.domains = v
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect();
        }
        if let Ok(v) = std::env::var(ENV_ACME_EMAIL) {
            self.contact_email = v;
        }
        if let Ok(v) = std::env::var(ENV_ACME_STAGING) {
            self.staging = matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes");
        }
        Ok(())
    }
}

/// Admin web panel settings. All fields have defaults; an empty
/// `[admin]` block in TOML is equivalent to "enable with defaults".
#[derive(Debug, Deserialize)]
pub struct AdminConfig {
    /// Interface to bind the admin HTTP listener to. Defaults to
    /// `127.0.0.1` so the admin surface stays loopback-only unless
    /// the operator explicitly opens it to the LAN — the panel is
    /// HTTP-only (no TLS) in v1, so exposing it publicly is a
    /// deliberate choice.
    #[serde(default = "default_admin_bind")]
    pub bind: String,
    /// Port for the admin HTTP listener. Default `8000` to match
    /// the spec; freely changeable without touching the gRPC port.
    #[serde(default = "default_admin_port")]
    pub port: u16,
    /// SQLite path for the admin user + session store. Relative
    /// paths are resolved against the process CWD, the same way
    /// `tls/cert.pem` is.
    #[serde(default = "default_admin_db_path")]
    pub db_path: PathBuf,
    /// Session TTL in hours. Cookies issued by `/api/login` are
    /// valid for this long; on expiry the next API call returns
    /// 401 and the JS shell re-prompts for credentials.
    #[serde(default = "default_admin_session_ttl_hours")]
    pub session_ttl_hours: u64,
    /// Optional plain-HTTP listener that 308-redirects every request
    /// to the HTTPS counterpart on `port`. The admin panel is TLS-only
    /// (gRPC and admin share the same cert), so a browser that lands
    /// on `http://host:8000` would otherwise see a raw TLS-handshake
    /// error. When this is `Some(n)`, the admin task binds a second
    /// listener on `bind:n` that responds to every request with a
    /// 308 Permanent Redirect to `https://<Host>:port<path>`. Default
    /// `None` — operators who don't want a second port keep their
    /// single-port setup. Common value: `8080`.
    #[serde(default)]
    pub http_redirect_port: Option<u16>,
}

impl Default for AdminConfig {
    fn default() -> Self {
        Self {
            bind: default_admin_bind(),
            port: default_admin_port(),
            db_path: default_admin_db_path(),
            session_ttl_hours: default_admin_session_ttl_hours(),
            http_redirect_port: None,
        }
    }
}

/// Environment-variable names for the `[admin]` overrides. Kept as
/// `const`s so the tests + docs reference them by symbol instead of
/// repeating string literals.
pub const ENV_ADMIN_BIND: &str = "TOKI_ADMIN_BIND";
pub const ENV_ADMIN_PORT: &str = "TOKI_ADMIN_PORT";
pub const ENV_ADMIN_DB_PATH: &str = "TOKI_ADMIN_DB_PATH";
pub const ENV_ADMIN_SESSION_TTL_HOURS: &str = "TOKI_ADMIN_SESSION_TTL_HOURS";
pub const ENV_ADMIN_HTTP_REDIRECT_PORT: &str = "TOKI_ADMIN_HTTP_REDIRECT_PORT";

/// Root directory for runtime-managed state (auto-generated TLS
/// certs, admin sqlite, anything else we write at boot or upgrade
/// time). Defaults to the process CWD (`.`) so a `cargo run` from a
/// fresh checkout keeps writing into the repo root, but Docker
/// images can pin it to `/data` and operators can move state under
/// `/var/lib/toki` without touching every individual path.
pub const ENV_DATA_DIR: &str = "TOKI_DATA_DIR";

/// Resolve the data-root directory: `$TOKI_DATA_DIR` if set,
/// otherwise `.`. Relative paths are kept relative; the resolver
/// just hands back whatever the env says (the caller is responsible
/// for `create_dir_all` / canonicalisation if it cares).
pub fn data_dir() -> PathBuf {
    std::env::var(ENV_DATA_DIR)
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

/// Join `base` and `path`, treating an absolute `path` as a hard
/// override (so operators who set `db_path = "/var/lib/toki/admin.db"`
/// keep that path verbatim regardless of `TOKI_DATA_DIR`). Relative
/// paths get the data-dir prefix so the auto-generated defaults land
/// under the data root.
pub fn resolve_under(base: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

impl AdminConfig {
    /// Apply `TOKI_ADMIN_*` env vars on top of the TOML-loaded
    /// values. Each var, when set, replaces the corresponding field
    /// — env beats TOML beats hard-coded defaults, matching the
    /// twelve-factor pattern used elsewhere in the binary (e.g.
    /// `TOKI_GRPC_ADDR`).
    ///
    /// A malformed value (non-numeric port, etc.) returns an error
    /// so the operator gets a clear startup failure rather than a
    /// silent fallback to the TOML value — same posture as the
    /// other `.parse()?` calls in `main.rs`.
    ///
    /// Recognised vars:
    /// * `TOKI_ADMIN_BIND` — interface, e.g. `0.0.0.0`
    /// * `TOKI_ADMIN_PORT` — TCP port, e.g. `8000`
    /// * `TOKI_ADMIN_DB_PATH` — sqlite path
    /// * `TOKI_ADMIN_SESSION_TTL_HOURS` — positive integer
    /// * `TOKI_ADMIN_HTTP_REDIRECT_PORT` — TCP port for the plain-HTTP
    ///   redirect listener; empty string disables it
    pub fn apply_env_overrides(&mut self) -> anyhow::Result<()> {
        if let Ok(v) = std::env::var(ENV_ADMIN_BIND) {
            self.bind = v;
        }
        if let Ok(v) = std::env::var(ENV_ADMIN_PORT) {
            self.port = v.parse().with_context(|| {
                format!("{ENV_ADMIN_PORT}={v:?}: expected a TCP port (0..=65535)")
            })?;
        }
        if let Ok(v) = std::env::var(ENV_ADMIN_DB_PATH) {
            self.db_path = PathBuf::from(v);
        }
        if let Ok(v) = std::env::var(ENV_ADMIN_SESSION_TTL_HOURS) {
            self.session_ttl_hours = v.parse().with_context(|| {
                format!("{ENV_ADMIN_SESSION_TTL_HOURS}={v:?}: expected a positive integer")
            })?;
        }
        if let Ok(v) = std::env::var(ENV_ADMIN_HTTP_REDIRECT_PORT) {
            // Empty string disables the redirect listener — handy in
            // Docker / systemd where unsetting an inherited env var
            // isn't always possible.
            self.http_redirect_port = if v.trim().is_empty() {
                None
            } else {
                Some(v.parse().with_context(|| {
                    format!("{ENV_ADMIN_HTTP_REDIRECT_PORT}={v:?}: expected a TCP port (0..=65535)")
                })?)
            };
        }
        Ok(())
    }
}

fn default_admin_bind() -> String {
    "127.0.0.1".to_string()
}
fn default_admin_port() -> u16 {
    8000
}
fn default_admin_db_path() -> PathBuf {
    PathBuf::from("admin.db")
}
fn default_admin_session_ttl_hours() -> u64 {
    12
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
    ///
    /// `TOKI_ADMIN_*` env vars are applied as the final overlay
    /// (env > TOML > defaults), so any deployment can tweak the
    /// admin panel's bind / port / db path without touching the
    /// TOML.
    pub fn load() -> anyhow::Result<(Self, Option<PathBuf>)> {
        let (mut cfg, path) = match locate() {
            Some(path) => (Self::from_path(&path)?, Some(path)),
            None => (Self::default(), None),
        };
        cfg.admin.apply_env_overrides()?;
        cfg.acme.apply_env_overrides()?;
        cfg.acme.validate()?;
        Ok((cfg, path))
    }

    /// Parse a config file at `path`. Does *not* apply env-var
    /// overrides — that's `load`'s job. Tests use this directly to
    /// keep the env-var application out of their critical path.
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
    fn admin_block_round_trips_with_overrides() {
        let raw = r#"
            [admin]
            bind = "0.0.0.0"
            port = 9000
            db_path = "/var/lib/toki/admin.db"
            session_ttl_hours = 24
        "#;
        let cfg: Config = toml::from_str(raw).unwrap();
        assert_eq!(cfg.admin.bind, "0.0.0.0");
        assert_eq!(cfg.admin.port, 9000);
        assert_eq!(
            cfg.admin.db_path.to_string_lossy(),
            "/var/lib/toki/admin.db"
        );
        assert_eq!(cfg.admin.session_ttl_hours, 24);
    }

    #[test]
    fn admin_block_fills_defaults_when_empty() {
        // An empty `[admin]` block parses cleanly and inherits every
        // default, identical to omitting the block.
        let cfg: Config = toml::from_str("[admin]\n").unwrap();
        assert_eq!(cfg.admin.bind, "127.0.0.1");
        assert_eq!(cfg.admin.port, 8000);
        assert_eq!(cfg.admin.db_path.to_string_lossy(), "admin.db");
        assert_eq!(cfg.admin.session_ttl_hours, 12);
    }

    #[test]
    fn admin_defaults_apply_when_block_absent() {
        // The admin dashboard is always exposed. A config.toml that
        // doesn't mention `[admin]` still produces the default
        // bind/port/db_path/ttl — same as an empty `[admin]` block.
        let cfg: Config = toml::from_str("password = \"hunter2\"").unwrap();
        assert_eq!(cfg.admin.bind, "127.0.0.1");
        assert_eq!(cfg.admin.port, 8000);
        assert_eq!(cfg.admin.db_path.to_string_lossy(), "admin.db");
        assert_eq!(cfg.admin.session_ttl_hours, 12);
    }

    /// RAII guard that clears an env var on drop. Tests that set
    /// `TOKI_ADMIN_*` use this so a failure mid-test (panic before
    /// the explicit clear) doesn't leak state into the next case.
    struct EnvGuard(&'static str);
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            std::env::remove_var(self.0);
        }
    }
    fn set_env(key: &'static str, val: &str) -> EnvGuard {
        std::env::set_var(key, val);
        EnvGuard(key)
    }

    #[test]
    #[serial_test::serial]
    fn env_overrides_each_admin_field() {
        // All four TOKI_ADMIN_* vars set at once → every field
        // overridden, regardless of what TOML said (here: nothing).
        let _b = set_env(ENV_ADMIN_BIND, "0.0.0.0");
        let _p = set_env(ENV_ADMIN_PORT, "9090");
        let _d = set_env(ENV_ADMIN_DB_PATH, "/tmp/test-admin.db");
        let _t = set_env(ENV_ADMIN_SESSION_TTL_HOURS, "24");
        let mut admin = AdminConfig::default();
        admin.apply_env_overrides().unwrap();
        assert_eq!(admin.bind, "0.0.0.0");
        assert_eq!(admin.port, 9090);
        assert_eq!(admin.db_path.to_string_lossy(), "/tmp/test-admin.db");
        assert_eq!(admin.session_ttl_hours, 24);
    }

    #[test]
    #[serial_test::serial]
    fn env_leaves_unset_fields_alone() {
        // Only port is set in env; bind / db_path / ttl come from
        // the in-memory config (here, the TOML-loaded values).
        let _p = set_env(ENV_ADMIN_PORT, "9091");
        let mut admin = AdminConfig {
            bind: "1.2.3.4".into(),
            port: 8000,
            db_path: PathBuf::from("custom.db"),
            session_ttl_hours: 6,
            http_redirect_port: None,
        };
        admin.apply_env_overrides().unwrap();
        assert_eq!(admin.bind, "1.2.3.4");
        assert_eq!(admin.port, 9091);
        assert_eq!(admin.db_path.to_string_lossy(), "custom.db");
        assert_eq!(admin.session_ttl_hours, 6);
    }

    #[test]
    #[serial_test::serial]
    fn env_overrides_win_over_toml() {
        // Simulate the full `Config::load` flow: parse TOML, then
        // apply env. The env value must replace the TOML one.
        let _g = set_env(ENV_ADMIN_PORT, "7777");
        let raw = "[admin]\nport = 9000\n";
        let mut cfg: Config = toml::from_str(raw).unwrap();
        assert_eq!(cfg.admin.port, 9000); // pre-override
        cfg.admin.apply_env_overrides().unwrap();
        assert_eq!(cfg.admin.port, 7777); // env wins
    }

    #[test]
    #[serial_test::serial]
    fn env_malformed_port_is_fatal() {
        let _g = set_env(ENV_ADMIN_PORT, "definitely-not-a-number");
        let mut admin = AdminConfig::default();
        let err = admin.apply_env_overrides().unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("TOKI_ADMIN_PORT"), "msg = {msg}");
    }

    #[test]
    #[serial_test::serial]
    fn env_sets_http_redirect_port_and_empty_clears() {
        // Setting the env var arms the redirect listener.
        let _g = set_env(ENV_ADMIN_HTTP_REDIRECT_PORT, "8080");
        let mut admin = AdminConfig::default();
        admin.apply_env_overrides().unwrap();
        assert_eq!(admin.http_redirect_port, Some(8080));

        // Empty string explicitly disables it — useful in Docker /
        // systemd where unsetting an inherited env var is awkward.
        std::env::set_var(ENV_ADMIN_HTTP_REDIRECT_PORT, "");
        let mut admin = AdminConfig {
            http_redirect_port: Some(8080),
            ..AdminConfig::default()
        };
        admin.apply_env_overrides().unwrap();
        assert_eq!(admin.http_redirect_port, None);
    }

    #[test]
    #[serial_test::serial]
    fn env_malformed_http_redirect_port_is_fatal() {
        let _g = set_env(ENV_ADMIN_HTTP_REDIRECT_PORT, "not-a-port");
        let mut admin = AdminConfig::default();
        let err = admin.apply_env_overrides().unwrap_err();
        assert!(format!("{err:#}").contains("TOKI_ADMIN_HTTP_REDIRECT_PORT"));
    }

    #[test]
    #[serial_test::serial]
    fn env_malformed_ttl_is_fatal() {
        let _g = set_env(ENV_ADMIN_SESSION_TTL_HOURS, "-5");
        let mut admin = AdminConfig::default();
        let err = admin.apply_env_overrides().unwrap_err();
        assert!(format!("{err:#}").contains("TOKI_ADMIN_SESSION_TTL_HOURS"));
    }

    #[test]
    #[serial_test::serial]
    fn data_dir_defaults_to_dot_when_env_unset() {
        std::env::remove_var(ENV_DATA_DIR);
        assert_eq!(data_dir(), PathBuf::from("."));
    }

    #[test]
    #[serial_test::serial]
    fn data_dir_honours_env_override() {
        let _g = set_env(ENV_DATA_DIR, "/var/lib/toki");
        assert_eq!(data_dir(), PathBuf::from("/var/lib/toki"));
    }

    #[test]
    fn resolve_under_keeps_absolute_paths() {
        // Absolute paths from the operator stay untouched — the
        // data-dir prefix is for relative defaults only, so a TLS
        // cert pointed at `/etc/letsencrypt/...` doesn't accidentally
        // get rewritten to `./etc/letsencrypt/...`.
        let abs = Path::new("/etc/letsencrypt/cert.pem");
        assert_eq!(resolve_under(Path::new("/var/lib/toki"), abs), abs);
    }

    #[test]
    fn resolve_under_prefixes_relative_paths() {
        let rel = Path::new("admin.db");
        assert_eq!(
            resolve_under(Path::new("/var/lib/toki"), rel),
            PathBuf::from("/var/lib/toki/admin.db")
        );
    }

    #[test]
    fn acme_absent_is_disabled() {
        let cfg: Config = toml::from_str("password = \"x\"").unwrap();
        assert!(!cfg.acme.enabled);
        assert!(!cfg.acme.is_active());
        assert_eq!(cfg.acme.http_bind, "0.0.0.0:80");
    }

    #[test]
    fn acme_block_round_trips() {
        let raw = r#"
            [acme]
            enabled = true
            domains = ["toki.example.com", "alt.example.com"]
            contact_email = "ops@example.com"
            terms_of_service_agreed = true
            staging = true
        "#;
        let cfg: Config = toml::from_str(raw).unwrap();
        assert!(cfg.acme.is_active());
        assert_eq!(
            cfg.acme.domains,
            vec!["toki.example.com", "alt.example.com"]
        );
        assert_eq!(cfg.acme.contact_email, "ops@example.com");
        assert!(cfg.acme.staging);
        cfg.acme.validate().expect("valid active config");
    }

    #[test]
    fn acme_validate_requires_email_and_tos() {
        let no_email = AcmeConfig {
            enabled: true,
            domains: vec!["toki.example.com".into()],
            terms_of_service_agreed: true,
            ..AcmeConfig::default()
        };
        assert!(no_email.validate().is_err(), "missing email must fail");

        let no_tos = AcmeConfig {
            enabled: true,
            domains: vec!["toki.example.com".into()],
            contact_email: "ops@example.com".into(),
            terms_of_service_agreed: false,
            ..AcmeConfig::default()
        };
        assert!(no_tos.validate().is_err(), "ToS not agreed must fail");

        // Inactive (disabled) configs always validate, even if incomplete.
        assert!(AcmeConfig::default().validate().is_ok());
    }

    #[test]
    #[serial_test::serial]
    fn acme_env_overrides() {
        let _e = set_env(ENV_ACME_ENABLED, "true");
        let _d = set_env(ENV_ACME_DOMAINS, " a.example.com , b.example.com ");
        let _m = set_env(ENV_ACME_EMAIL, "ops@example.com");
        let _s = set_env(ENV_ACME_STAGING, "1");
        let mut acme = AcmeConfig::default();
        acme.apply_env_overrides().unwrap();
        assert!(acme.enabled);
        assert_eq!(acme.domains, vec!["a.example.com", "b.example.com"]);
        assert_eq!(acme.contact_email, "ops@example.com");
        assert!(acme.staging);
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
