//! JSON-over-the-wire shapes for the admin API.
//!
//! These types are the *only* contract the embedded JS depends on —
//! field names here are also field names in `app.js`. Keep them
//! camelCase to match JS conventions on the consumer side and
//! `#[serde(rename_all = "camelCase")]` here.
//!
//! The shapes are deliberately denormalised (we send display names
//! inline next to client ids, etc.) so the UI doesn't have to do
//! lookups across response chunks. Snapshots are small (a few
//! kilobytes for a busy server) so the bandwidth cost is negligible.

use serde::{Deserialize, Serialize};

/// Top-level snapshot of server state. One of these is emitted by
/// the SSE broadcaster ~1Hz and also returned synchronously by
/// `GET /api/state`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    /// All clients with `current_frequency == Some(_)`, grouped by
    /// frequency. Frequencies are canonical strings ("446.05"), the
    /// same form clients send in `JoinRequest`.
    pub rooms: Vec<RoomDto>,
    /// Clients that are registered but not currently in any room
    /// (post-Register, pre-Join, or post-Leave but not yet expired).
    /// Useful for the admin to see ghosts that the reaper hasn't
    /// claimed yet.
    pub lobby: Vec<MemberDto>,
    /// Generation counter — incremented by the broadcaster on every
    /// emitted snapshot. The JS uses this to detect "we missed a
    /// frame" after an SSE reconnect (just re-renders the current
    /// snapshot — every frame is self-contained).
    pub generation: u64,
    /// Seconds since the server process booted. Drives the header
    /// "UPTIME 4d 07:22:14" ticker. Recomputed every snapshot rather
    /// than incremented in JS so a missed SSE frame can't drift.
    pub server_uptime_secs: u64,
}

/// One-shot server identity payload returned by `GET /api/server-info`.
/// Fields here change at most across restarts, so the UI fetches once
/// at dashboard mount and never again. Keeps the per-tick snapshot
/// small and avoids the overhead of re-serialising config on every
/// broadcast.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerInfo {
    /// Crate version (`env!("CARGO_PKG_VERSION")` at compile time).
    /// Rendered in the header next to "ADMIN".
    pub version: String,
    /// The admin panel's bound address, e.g. `127.0.0.1:8000`.
    /// Shown as the "HOST" stat in the header — gives the operator
    /// a sanity check that they're looking at the panel they think
    /// they are, especially handy with reverse-proxy setups.
    pub admin_bind: String,
    /// Unix timestamp of process startup. The Overview KPI strip
    /// renders this as "since YYYY-MM-DD" under the live uptime.
    pub started_at_unix: u64,
    /// `true` when `config.toml` provided a `password` at startup.
    /// In that mode the gRPC password is locked by the bootstrap
    /// layer and the admin UI disables its server-password input
    /// (operators who want to manage it from the panel should
    /// remove the line from config.toml and restart). `false` when
    /// the password — if any — comes from the runtime db only.
    pub toml_password_override: bool,
}

/// Body of `PUT /api/server-config`. Note: covers only the runtime
/// tunables shown on the "Runtime" card. The gRPC server password
/// has its own endpoint (`PUT /api/server-password`) because the
/// UI surfaces it as a separate card with different controls + a
/// TOML-override lock state.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateServerConfigRequest {
    pub server_name: String,
    pub max_peers: u32,
    pub idle_kick_secs: u32,
}

/// Body of `PUT /api/server-password`. The empty string disarms the
/// gate (open mode); any non-empty value arms it. Operators who set
/// the password via `config.toml` get a 409 here — the TOML
/// override owns the field until they remove that line.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateServerPasswordRequest {
    pub password: String,
}

/// One frequency room. The `members` list always includes the
/// `holder` (if any); the UI highlights the holder rather than
/// rendering them in a separate list.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RoomDto {
    pub frequency: String,
    /// Client id of the current PTT holder, if any. Matches one of
    /// the `members[i].id` values.
    pub holder: Option<String>,
    pub members: Vec<MemberDto>,
}

/// One client, as seen by the admin UI. Carries enough to render the
/// member row and call the mutation endpoints (id is the key for
/// every `/api/clients/:id/*` route).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemberDto {
    pub id: String,
    pub display_name: String,
    /// Seconds since the client's last UDP packet. The UI surfaces
    /// this as "10s ago" so the operator can spot a near-zombie
    /// before the reaper catches it.
    pub last_seen_secs: u64,
}

/// Body of `POST /api/login`.
#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
}

/// Body of `POST /api/clients/:id/move`.
#[derive(Debug, Deserialize)]
pub struct MoveRequest {
    pub frequency: String,
}

/// Body of `POST /api/clients/:id/rename`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RenameRequest {
    pub display_name: String,
}

/// Body of `POST /api/account/password`.
///
/// `current` is verified against the stored argon2 hash before the
/// change goes through — even an attacker who's hijacked the session
/// cookie has to know the cleartext to rotate it.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChangePasswordRequest {
    pub current: String,
    pub new: String,
}

/// Standard error envelope returned by every fallible handler.
/// The JS shows `error` to the operator verbatim; keep messages
/// terse and operator-friendly.
#[derive(Debug, Serialize)]
pub struct ApiError {
    pub error: String,
}

impl ApiError {
    pub fn new(msg: impl Into<String>) -> Self {
        Self { error: msg.into() }
    }
}
