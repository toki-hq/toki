//! Runtime-mutable server settings.
//!
//! Lives in `admin.db` (when the admin panel is enabled) and is loaded
//! into an `Arc<tokio::sync::RwLock<ServerConfig>>` at startup. The
//! gRPC signaling service and the reaper read it on each request /
//! tick; the admin panel's `PUT /api/server-config` handler atomically
//! updates both the row in sqlite *and* the in-memory copy so the new
//! values take effect without a restart.
//!
//! # Why a singleton, not key/value
//!
//! A single typed row gives us SQL-level type safety (no parsing
//! strings on every read) and a fixed schema the admin UI can reason
//! about. The cost — one migration per new field — is small at this
//! stage. If the table ever grows past a dozen fields we'd revisit.
//!
//! # Why a shared `Arc<RwLock>` rather than reload-from-db on read
//!
//! Reads are on the hot path (every Register call, every reaper
//! tick). A db round-trip per read would be wasteful and would
//! couple the gRPC handler to sqlite availability. The RwLock keeps
//! reads uncontended and lets the admin handler do a single batched
//! write under the lock.
//!
//! # Bootstrapping when admin isn't enabled
//!
//! Headless deployments (no `[admin]` block in `config.toml`) never
//! open `admin.db`. We still construct the shared `ServerConfig` —
//! at `Default::default()` values — so signaling + reaper get sane
//! limits in that path. They just can't be edited without enabling
//! the admin panel.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

/// One row, all the dials. Add fields here, then bump the DB
/// migration in `admin/db.rs`. Field documentation here is also
/// the API documentation — these field names land on the wire
/// verbatim as JSON (camelCase via serde).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerConfig {
    /// Human-readable name for this Toki deployment. Displayed in
    /// the admin panel's Overview header and (in a future release)
    /// echoed back to clients on connect. Empty string means "no
    /// name set" — the UI then shows just the host:port pair.
    pub server_name: String,

    /// Hard ceiling on `Registry.clients.len()`. Once reached, the
    /// gRPC `Register` RPC rejects new registrations with
    /// `RESOURCE_EXHAUSTED`. Protects against unbounded registry
    /// growth from misbehaving clients or a memory-amplification
    /// probe. Set to a sane default that fits a single small VPS;
    /// operators with bigger boxes will want to bump it.
    pub max_peers: u32,

    /// Eviction threshold for the stale-client reaper, in seconds.
    /// A client that hasn't sent a keepalive in this long is
    /// removed from the registry and its room peers are notified
    /// via `MemberLeft`. Lower values reap zombies faster at the
    /// cost of false-positive evictions on a flaky network; the
    /// default tolerates two missed keepalives plus jitter.
    pub idle_kick_secs: u32,

    /// Shared-secret password the gRPC `Register` RPC requires
    /// from connecting Toki clients. Empty string means open mode
    /// (no password gate). The signaling service consults this
    /// only if no `password` line is set in `config.toml`; when
    /// TOML has one, it wins (see [`admin::AppState`] for the
    /// override flag the UI consults to lock its input).
    ///
    /// Stored cleartext because both endpoints of the comparison
    /// need the same value — the gRPC client sends cleartext too —
    /// and the admin db is already chmod-0600 with operator-only
    /// access. Argon2 here would only break the comparison.
    pub grpc_password: String,
}

impl Default for ServerConfig {
    fn default() -> Self {
        // These defaults are the same values the code used to ship
        // hardcoded — moving them into the DB doesn't change any
        // observable behaviour on a fresh deployment. They become
        // mutable from this point forward.
        Self {
            server_name: String::new(),
            max_peers: 256,
            idle_kick_secs: 10,
            grpc_password: String::new(),
        }
    }
}

/// Type alias for the shared handle plumbed through `main` to
/// signaling, the reaper, and the admin task. Cheap to clone; reads
/// are `read().await` on the RwLock + clone of the small struct,
/// writes are a single `write().await` from the admin save handler.
pub type SharedServerConfig = Arc<RwLock<ServerConfig>>;

/// Build a fresh shared handle with default values. Called from
/// `main` before the admin task gets a chance to overwrite from db.
pub fn shared_default() -> SharedServerConfig {
    Arc::new(RwLock::new(ServerConfig::default()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_values_match_legacy_constants() {
        // The hardcoded constants we're replacing must match the
        // defaults exactly so a deploy that doesn't enable admin
        // behaves identically to before this change.
        let d = ServerConfig::default();
        assert_eq!(d.max_peers, 256);
        assert_eq!(d.idle_kick_secs, 10);
        assert_eq!(d.server_name, "");
        assert_eq!(d.grpc_password, "", "default to open mode");
    }

    #[test]
    fn serialisation_round_trips() {
        // Wire format is JSON via the admin API; verify a round
        // trip keeps all fields and the camelCase rename.
        let original = ServerConfig {
            server_name: "Singular Toki".into(),
            max_peers: 1024,
            idle_kick_secs: 30,
            grpc_password: "hunter2".into(),
        };
        let json = serde_json::to_string(&original).unwrap();
        assert!(json.contains("\"serverName\":\"Singular Toki\""));
        assert!(json.contains("\"maxPeers\":1024"));
        assert!(json.contains("\"idleKickSecs\":30"));
        assert!(json.contains("\"grpcPassword\":\"hunter2\""));
        let parsed: ServerConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.server_name, original.server_name);
        assert_eq!(parsed.max_peers, original.max_peers);
        assert_eq!(parsed.idle_kick_secs, original.idle_kick_secs);
        assert_eq!(parsed.grpc_password, original.grpc_password);
    }
}
