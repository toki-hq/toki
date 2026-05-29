//! SQLite-backed admin user + session store.
//!
//! Two tables, both created idempotently on first boot via [`AdminDb::migrate`]:
//!
//! * `admin_users(username PK, password_hash, created_at)` — one row per
//!   admin account. We never read passwords in cleartext; only the
//!   argon2id hash is stored. v1 ships with a single seeded `admin` user
//!   and no UI to create more (deliberate — multi-admin is a follow-up).
//!
//! * `sessions(token_hash PK, username, expires_at)` — issued by
//!   `/api/login`. The *cookie* on the wire carries a raw 16-byte
//!   token (hex-encoded, CSPRNG from `rand::rngs::OsRng`); the *db*
//!   stores `BLAKE3(token)` as a 64-char hex string. Lookup hashes
//!   the cookie value and matches against the column. This closes
//!   the "leaked admin.db lets you replay live sessions" hole — an
//!   attacker with read access to the file gets only the hash, which
//!   is preimage-resistant.
//!
//! All connection access goes through a `std::sync::Mutex` wrapped in
//! `Arc` and offloaded to `spawn_blocking`. Admin traffic is on the order
//! of single-digit requests per second, so the lock is uncontended; the
//! `spawn_blocking` hop just keeps the sqlite syscalls off the async
//! executor.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};

use crate::server_config::ServerConfig;

/// Thin async wrapper around a single sqlite connection.
///
/// `Clone` is cheap (Arc bump) and intentional — every axum handler
/// pulls a clone of `AppState` and operates on the same connection.
#[derive(Clone)]
pub struct AdminDb {
    conn: Arc<std::sync::Mutex<Connection>>,
}

impl AdminDb {
    /// Open (or create) the sqlite file at `path`. Does *not* run
    /// migrations — call [`migrate`](Self::migrate) explicitly so
    /// callers can fail fast on schema errors.
    ///
    /// On Unix, tightens the file mode to `0600` (owner-only RW)
    /// before returning. The file holds argon2 hashes + session
    /// tokens — both useful to an attacker with read access — so
    /// we don't want to inherit the operator's umask (typically
    /// `022`, which would leave the file world-readable). Mirrors
    /// the treatment of the gRPC TLS private key in `tls.rs`.
    pub fn open(path: &Path) -> Result<Self> {
        // Make sure the parent directory exists. Operators usually
        // configure something like `/var/lib/toki/admin.db`; without
        // this we'd error out on first boot rather than auto-creating.
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("create admin db parent {}", parent.display()))?;
            }
        }
        let conn =
            Connection::open(path).with_context(|| format!("open sqlite {}", path.display()))?;
        // chmod after open so the file definitely exists. Best-effort:
        // a chmod failure logs a warning rather than aborting startup
        // (matches the TLS-key behaviour and stays consistent with how
        // the gRPC side handles its private-key permissions).
        tighten_db_perms(path);
        Ok(Self {
            conn: Arc::new(std::sync::Mutex::new(conn)),
        })
    }

    /// Open an in-memory connection. Used by tests (both unit and
    /// integration) so a case can exercise the full migration +
    /// query path without touching the filesystem. Public — the
    /// function is harmless in production and `pub(crate)` would
    /// hide it from integration tests in `tests/`.
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().context("open in-memory sqlite")?;
        Ok(Self {
            conn: Arc::new(std::sync::Mutex::new(conn)),
        })
    }

    /// Apply the schema. Idempotent: re-running on an already-migrated
    /// db is a no-op. We use `IF NOT EXISTS` rather than a version
    /// table because v1 has exactly one schema; a real migration
    /// framework would be premature.
    ///
    /// The `sessions` table stores `token_hash` (BLAKE3 of the cookie
    /// value), never the raw token — see module-level docs for why.
    /// If an older binary populated this table with raw tokens, the
    /// rows will be unmatchable on next login (their hashes won't
    /// equal the stored raw values) and the operator will simply be
    /// asked to log in again. Acceptable one-time UX cost for the
    /// security upgrade; we don't migrate ancient rows.
    pub async fn migrate(&self) -> Result<()> {
        self.with_conn(|c| {
            c.execute_batch(
                r#"
                CREATE TABLE IF NOT EXISTS admin_users (
                    username      TEXT PRIMARY KEY NOT NULL,
                    password_hash TEXT NOT NULL,
                    created_at    INTEGER NOT NULL
                );
                CREATE TABLE IF NOT EXISTS sessions (
                    token_hash TEXT PRIMARY KEY NOT NULL,
                    username   TEXT NOT NULL,
                    expires_at INTEGER NOT NULL,
                    FOREIGN KEY (username) REFERENCES admin_users(username)
                );
                CREATE INDEX IF NOT EXISTS sessions_expires_idx
                    ON sessions(expires_at);

                -- Runtime-mutable server settings. Singleton — the
                -- `CHECK (id = 1)` makes the one-row invariant visible
                -- and rejects attempts to add a second row. Defaults
                -- track the legacy hardcoded constants exactly, so a
                -- fresh db behaves identically to a pre-server_config
                -- build.
                CREATE TABLE IF NOT EXISTS server_config (
                    id              INTEGER PRIMARY KEY CHECK (id = 1),
                    server_name     TEXT    NOT NULL DEFAULT '',
                    max_peers       INTEGER NOT NULL DEFAULT 256,
                    idle_kick_secs  INTEGER NOT NULL DEFAULT 10,
                    grpc_password   TEXT    NOT NULL DEFAULT '',
                    updated_at      INTEGER NOT NULL DEFAULT 0
                );
                INSERT OR IGNORE INTO server_config (id) VALUES (1);

                -- Admin-assigned channel names, keyed by canonical
                -- frequency string. Rows persist independently of room
                -- occupancy (a name outlives the last member leaving).
                -- Absence of a row = unnamed. Gated at runtime by
                -- server_config.named_channels_enabled.
                CREATE TABLE IF NOT EXISTS channel_names (
                    frequency  TEXT PRIMARY KEY NOT NULL,
                    name       TEXT NOT NULL,
                    updated_at INTEGER NOT NULL DEFAULT 0
                );

                -- Time-series metrics, one row per minute (see metrics.rs).
                -- `ts` is unix seconds; rx/tx are bytes/sec averaged over
                -- the sample interval. Pruned past the retention window.
                CREATE TABLE IF NOT EXISTS metrics_samples (
                    ts           INTEGER PRIMARY KEY NOT NULL,
                    rx_bps       INTEGER NOT NULL,
                    tx_bps       INTEGER NOT NULL,
                    users        INTEGER NOT NULL,
                    transmitting INTEGER NOT NULL
                );

                -- Audit log: admin actions, security/auth events, and peer
                -- connect/disconnect. Append-only; paged newest-first by id.
                CREATE TABLE IF NOT EXISTS audit_log (
                    id        INTEGER PRIMARY KEY AUTOINCREMENT,
                    ts        INTEGER NOT NULL,
                    kind      TEXT NOT NULL,
                    actor     TEXT NOT NULL,
                    frequency TEXT NOT NULL DEFAULT '',
                    detail    TEXT NOT NULL DEFAULT ''
                );
                CREATE INDEX IF NOT EXISTS audit_log_id_idx ON audit_log(id);
                CREATE INDEX IF NOT EXISTS metrics_ts_idx ON metrics_samples(ts);
                "#,
            )?;
            // Upgrade path: a db that pre-dates the grpc_password
            // column won't get it from `CREATE TABLE IF NOT EXISTS`
            // (the table already exists). Add it explicitly via a
            // `pragma_table_info` check so the call stays idempotent
            // — we can't lean on `ALTER TABLE … IF NOT EXISTS`
            // because sqlite doesn't support that conditional form.
            ensure_column_exists(
                c,
                "server_config",
                "grpc_password",
                "TEXT NOT NULL DEFAULT ''",
            )?;
            // Upgrade path for the named-channels toggle (added after
            // the grpc_password column). Same idempotent ALTER dance.
            ensure_column_exists(
                c,
                "server_config",
                "named_channels_enabled",
                "INTEGER NOT NULL DEFAULT 0",
            )?;
            Ok(())
        })
        .await
    }

    /// Read the singleton `server_config` row. Returns `Default` if
    /// the row hasn't been touched yet (i.e. on the very first read
    /// after migration); the migration's `INSERT OR IGNORE` already
    /// created it with default column values, so this branch is
    /// mostly defensive.
    pub async fn load_server_config(&self) -> Result<ServerConfig> {
        self.with_conn(|c| {
            let row = c
                .query_row(
                    "SELECT server_name, max_peers, idle_kick_secs, grpc_password, \
                     named_channels_enabled \
                     FROM server_config WHERE id = 1",
                    [],
                    |r| {
                        Ok(ServerConfig {
                            server_name: r.get(0)?,
                            max_peers: r.get::<_, i64>(1)? as u32,
                            idle_kick_secs: r.get::<_, i64>(2)? as u32,
                            grpc_password: r.get(3)?,
                            named_channels_enabled: r.get::<_, i64>(4)? != 0,
                        })
                    },
                )
                .optional()?;
            Ok(row.unwrap_or_default())
        })
        .await
    }

    /// Persist the server config back to the singleton row. Stamps
    /// `updated_at` to the current unix time so we can tell when a
    /// setting last changed (useful for the audit log follow-up).
    pub async fn save_server_config(&self, cfg: &ServerConfig) -> Result<()> {
        let cfg = cfg.clone();
        let now = now_unix();
        self.with_conn(move |c| {
            c.execute(
                "UPDATE server_config \
                 SET server_name = ?1, max_peers = ?2, idle_kick_secs = ?3, \
                     grpc_password = ?4, named_channels_enabled = ?5, updated_at = ?6 \
                 WHERE id = 1",
                params![
                    cfg.server_name,
                    cfg.max_peers as i64,
                    cfg.idle_kick_secs as i64,
                    cfg.grpc_password,
                    cfg.named_channels_enabled as i64,
                    now,
                ],
            )?;
            Ok(())
        })
        .await
    }

    /// Load every channel name into a `frequency → name` map. Called
    /// once at startup to seed the in-memory [`SharedChannelNames`];
    /// the admin mutation handlers keep that map and the table in sync
    /// thereafter, so this is never on a hot path.
    pub async fn load_channel_names(&self) -> Result<std::collections::HashMap<String, String>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare("SELECT frequency, name FROM channel_names")?;
            let rows =
                stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
            let mut map = std::collections::HashMap::new();
            for row in rows {
                let (freq, name) = row?;
                map.insert(freq, name);
            }
            Ok(map)
        })
        .await
    }

    /// Upsert a single channel name (caller has already validated the
    /// frequency is canonical and the name is ≤16 chars).
    pub async fn set_channel_name(&self, frequency: &str, name: &str) -> Result<()> {
        let frequency = frequency.to_string();
        let name = name.to_string();
        let now = now_unix();
        self.with_conn(move |c| {
            c.execute(
                "INSERT INTO channel_names (frequency, name, updated_at) VALUES (?1, ?2, ?3) \
                 ON CONFLICT(frequency) DO UPDATE SET name = ?2, updated_at = ?3",
                params![frequency, name, now],
            )?;
            Ok(())
        })
        .await
    }

    /// Delete a single channel name (clearing it). No-op if absent.
    pub async fn clear_channel_name(&self, frequency: &str) -> Result<()> {
        let frequency = frequency.to_string();
        self.with_conn(move |c| {
            c.execute(
                "DELETE FROM channel_names WHERE frequency = ?1",
                params![frequency],
            )?;
            Ok(())
        })
        .await
    }

    /// Delete every channel name in one statement.
    pub async fn clear_all_channel_names(&self) -> Result<()> {
        self.with_conn(|c| {
            c.execute("DELETE FROM channel_names", [])?;
            Ok(())
        })
        .await
    }

    // ── Metrics time-series ───────────────────────────────────────

    /// Append one metrics sample (1-minute cadence). `INSERT OR REPLACE`
    /// keeps the `ts` PK unique if two ticks ever land in the same second.
    pub async fn insert_metric_sample(
        &self,
        ts: i64,
        rx_bps: u64,
        tx_bps: u64,
        users: u32,
        transmitting: u32,
    ) -> Result<()> {
        self.with_conn(move |c| {
            c.execute(
                "INSERT OR REPLACE INTO metrics_samples (ts, rx_bps, tx_bps, users, transmitting) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    ts,
                    rx_bps as i64,
                    tx_bps as i64,
                    users as i64,
                    transmitting as i64
                ],
            )?;
            Ok(())
        })
        .await
    }

    /// Load samples with `ts >= since`, oldest-first.
    pub async fn load_metrics(&self, since: i64) -> Result<Vec<MetricRow>> {
        self.with_conn(move |c| {
            let mut stmt = c.prepare(
                "SELECT ts, rx_bps, tx_bps, users, transmitting FROM metrics_samples \
                 WHERE ts >= ?1 ORDER BY ts ASC",
            )?;
            let rows = stmt.query_map(params![since], |r| {
                Ok(MetricRow {
                    ts: r.get(0)?,
                    rx_bps: r.get::<_, i64>(1)? as u64,
                    tx_bps: r.get::<_, i64>(2)? as u64,
                    users: r.get::<_, i64>(3)? as u32,
                    transmitting: r.get::<_, i64>(4)? as u32,
                })
            })?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })
        .await
    }

    /// Delete metrics rows older than `cutoff` (unix seconds).
    pub async fn prune_metrics(&self, cutoff: i64) -> Result<()> {
        self.with_conn(move |c| {
            c.execute("DELETE FROM metrics_samples WHERE ts < ?1", params![cutoff])?;
            Ok(())
        })
        .await
    }

    // ── Audit log ─────────────────────────────────────────────────

    /// Append an audit entry. Caller supplies the unix timestamp so the
    /// recorder controls clock semantics.
    pub async fn insert_audit(
        &self,
        ts: i64,
        kind: &str,
        actor: &str,
        frequency: &str,
        detail: &str,
    ) -> Result<()> {
        let (kind, actor, frequency, detail) = (
            kind.to_string(),
            actor.to_string(),
            frequency.to_string(),
            detail.to_string(),
        );
        self.with_conn(move |c| {
            c.execute(
                "INSERT INTO audit_log (ts, kind, actor, frequency, detail) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![ts, kind, actor, frequency, detail],
            )?;
            Ok(())
        })
        .await
    }

    /// Page the audit log newest-first. `kinds` is the set of allowed
    /// `kind` values for the active filter (empty = no filter / ALL).
    /// `before_id` of 0 means the newest page; otherwise rows with
    /// `id < before_id`. Returns `(rows, total_matching)`.
    pub async fn load_audit(
        &self,
        kinds: &[&str],
        limit: u32,
        before_id: u64,
    ) -> Result<(Vec<AuditRow>, u64)> {
        // Build the optional `kind IN (...)` clause from a fixed, code-
        // supplied vocabulary (never user input) — safe to interpolate.
        let kind_list: Vec<String> = kinds.iter().map(|k| format!("'{k}'")).collect();
        let kind_clause = if kind_list.is_empty() {
            String::new()
        } else {
            format!(" AND kind IN ({})", kind_list.join(","))
        };
        let before_clause = if before_id > 0 {
            format!(" AND id < {before_id}")
        } else {
            String::new()
        };
        let limit = limit.clamp(1, 500);
        self.with_conn(move |c| {
            let total: i64 = c.query_row(
                &format!("SELECT COUNT(*) FROM audit_log WHERE 1=1{kind_clause}"),
                [],
                |r| r.get(0),
            )?;
            let sql = format!(
                "SELECT id, ts, kind, actor, frequency, detail FROM audit_log \
                 WHERE 1=1{kind_clause}{before_clause} ORDER BY id DESC LIMIT {limit}"
            );
            let mut stmt = c.prepare(&sql)?;
            let rows = stmt.query_map([], |r| {
                Ok(AuditRow {
                    id: r.get::<_, i64>(0)? as u64,
                    ts: r.get(1)?,
                    kind: r.get(2)?,
                    actor: r.get(3)?,
                    frequency: r.get(4)?,
                    detail: r.get(5)?,
                })
            })?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok((out, total as u64))
        })
        .await
    }

    /// Delete audit rows older than `cutoff` (unix seconds).
    pub async fn prune_audit(&self, cutoff: i64) -> Result<()> {
        self.with_conn(move |c| {
            c.execute("DELETE FROM audit_log WHERE ts < ?1", params![cutoff])?;
            Ok(())
        })
        .await
    }

    /// Count rows in `admin_users`. The seeder uses this to decide
    /// whether to mint the bootstrap account.
    pub async fn user_count(&self) -> Result<i64> {
        self.with_conn(|c| {
            let n: i64 = c.query_row("SELECT COUNT(*) FROM admin_users", [], |r| r.get(0))?;
            Ok(n)
        })
        .await
    }

    /// Insert a new admin user. Returns an error if the username
    /// already exists — the seeder only calls this when the table
    /// is empty, so collisions indicate a logic bug.
    pub async fn insert_user(&self, username: &str, password_hash: &str) -> Result<()> {
        let username = username.to_string();
        let password_hash = password_hash.to_string();
        self.with_conn(move |c| {
            let now = now_unix();
            c.execute(
                "INSERT INTO admin_users (username, password_hash, created_at) VALUES (?1, ?2, ?3)",
                params![username, password_hash, now],
            )?;
            Ok(())
        })
        .await
    }

    /// Replace the stored argon2 hash for an existing user. Used by
    /// the `/api/account/password` change flow. Silently no-ops on a
    /// missing user — callers always verify existence (via the
    /// session middleware) before reaching this method, so a zero-
    /// row update would indicate a logic bug we'd rather not paper
    /// over by erroring.
    pub async fn update_password_hash(&self, username: &str, new_hash: &str) -> Result<()> {
        let username = username.to_string();
        let new_hash = new_hash.to_string();
        self.with_conn(move |c| {
            c.execute(
                "UPDATE admin_users SET password_hash = ?1 WHERE username = ?2",
                params![new_hash, username],
            )?;
            Ok(())
        })
        .await
    }

    /// Delete every session for `username` **except** the one whose
    /// hash matches `keep_token_hash`. Returns the number of rows
    /// removed. Used after a successful password change so any
    /// already-issued cookies that aren't the current browser's get
    /// invalidated — if the password change is happening because of
    /// a suspected compromise, the attacker's parallel session dies
    /// the moment this query commits.
    pub async fn delete_other_sessions_for_user(
        &self,
        username: &str,
        keep_token_hash: &str,
    ) -> Result<u64> {
        let username = username.to_string();
        let keep = keep_token_hash.to_string();
        self.with_conn(move |c| {
            let n = c.execute(
                "DELETE FROM sessions WHERE username = ?1 AND token_hash != ?2",
                params![username, keep],
            )?;
            Ok(n as u64)
        })
        .await
    }

    /// Look up the password hash for a given username. Returns `None`
    /// if the user doesn't exist. Login handlers run a constant-time
    /// argon2 verify against this; the *presence* of a row is therefore
    /// not directly observable in normal timing (the verify dominates).
    pub async fn get_password_hash(&self, username: &str) -> Result<Option<String>> {
        let username = username.to_string();
        self.with_conn(move |c| {
            c.query_row(
                "SELECT password_hash FROM admin_users WHERE username = ?1",
                params![username],
                |r| r.get::<_, String>(0),
            )
            .optional()
        })
        .await
    }

    /// Insert a fresh session row. `expires_at` is a unix timestamp.
    /// The cookie value handed to the browser is `token`; on disk we
    /// store only `hash_session_token(token)` so the raw cookie value
    /// never lives in the sqlite file.
    pub async fn create_session(&self, token: &str, username: &str, expires_at: i64) -> Result<()> {
        let token_hash = hash_session_token(token);
        let username = username.to_string();
        self.with_conn(move |c| {
            c.execute(
                "INSERT INTO sessions (token_hash, username, expires_at) VALUES (?1, ?2, ?3)",
                params![token_hash, username, expires_at],
            )?;
            Ok(())
        })
        .await
    }

    /// Resolve a cookie token back to its (username, expiry). Returns
    /// `None` if the token is unknown *or* expired — callers don't need
    /// to differentiate; both map to 401. The cookie value is hashed
    /// before the query; the raw token never reaches sqlite.
    pub async fn lookup_session(&self, token: &str) -> Result<Option<SessionRow>> {
        let token_hash = hash_session_token(token);
        let now = now_unix();
        self.with_conn(move |c| {
            c.query_row(
                "SELECT username, expires_at FROM sessions \
                 WHERE token_hash = ?1 AND expires_at > ?2",
                params![token_hash, now],
                |r| {
                    Ok(SessionRow {
                        username: r.get(0)?,
                        expires_at: r.get(1)?,
                    })
                },
            )
            .optional()
        })
        .await
    }

    /// Drop a single session row. Idempotent — deleting an unknown
    /// token returns `Ok(())`. Called by `/api/logout`. Like the
    /// lookup, hashes the cookie value before matching.
    pub async fn delete_session(&self, token: &str) -> Result<()> {
        let token_hash = hash_session_token(token);
        self.with_conn(move |c| {
            c.execute(
                "DELETE FROM sessions WHERE token_hash = ?1",
                params![token_hash],
            )?;
            Ok(())
        })
        .await
    }

    /// Sweep expired session rows. Called opportunistically on each
    /// login so the table doesn't grow unbounded over weeks of
    /// browser-tab churn.
    pub async fn prune_expired_sessions(&self) -> Result<u64> {
        let now = now_unix();
        self.with_conn(move |c| {
            let n = c.execute("DELETE FROM sessions WHERE expires_at <= ?1", params![now])?;
            Ok(n as u64)
        })
        .await
    }

    /// Run a closure with locked, blocking access to the sqlite
    /// connection on a worker thread. Every public method goes
    /// through this so the async signatures stay uniform.
    ///
    /// `pub(crate)` because the integration tests in `tests/admin.rs`
    /// poke the db directly to assert invariants the public API
    /// would otherwise hide (e.g. the H2 "raw token doesn't land on
    /// disk" check). Not part of the public surface.
    pub(crate) async fn with_conn<F, R>(&self, f: F) -> Result<R>
    where
        F: FnOnce(&Connection) -> rusqlite::Result<R> + Send + 'static,
        R: Send + 'static,
    {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn
                .lock()
                .map_err(|_| anyhow::anyhow!("admin sqlite mutex poisoned"))?;
            f(&conn).map_err(anyhow::Error::from)
        })
        .await
        .context("admin sqlite spawn_blocking join")?
    }
}

/// Result of a successful [`AdminDb::lookup_session`].
#[derive(Debug)]
pub struct SessionRow {
    pub username: String,
    pub expires_at: i64,
}

/// One metrics time-series row (see [`AdminDb::load_metrics`]).
#[derive(Debug, Clone)]
pub struct MetricRow {
    pub ts: i64,
    pub rx_bps: u64,
    pub tx_bps: u64,
    pub users: u32,
    pub transmitting: u32,
}

/// One audit-log row (see [`AdminDb::load_audit`]).
#[derive(Debug, Clone)]
pub struct AuditRow {
    pub id: u64,
    pub ts: i64,
    pub kind: String,
    pub actor: String,
    pub frequency: String,
    pub detail: String,
}

/// Idempotently add a column to a table. SQLite has no
/// `ALTER TABLE … ADD COLUMN IF NOT EXISTS`, so we check
/// `pragma_table_info(<table>)` first and skip the ALTER if the
/// column is already there. Used for schema upgrades from a db
/// created by an older binary — fresh dbs get the column via the
/// initial `CREATE TABLE` and this becomes a no-op.
fn ensure_column_exists(
    c: &Connection,
    table: &str,
    column: &str,
    spec: &str,
) -> rusqlite::Result<()> {
    let mut stmt = c.prepare(&format!("PRAGMA table_info({table})"))?;
    let exists = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .filter_map(|r| r.ok())
        .any(|name| name == column);
    if !exists {
        // `ALTER TABLE` doesn't accept `?` parameters for column
        // names, but `table`/`column`/`spec` are all hardcoded at
        // call sites — no untrusted input lands here.
        c.execute(
            &format!("ALTER TABLE {table} ADD COLUMN {column} {spec}"),
            [],
        )?;
    }
    Ok(())
}

/// Apply `chmod 0600` to the admin db file. Best-effort: a permission
/// error gets warned about but doesn't abort startup (so the panel
/// still comes up on platforms / filesystems where chmod is a no-op
/// or unsupported). Same posture as `tls::tighten_key_perms`.
#[cfg(unix)]
fn tighten_db_perms(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
        tracing::warn!(error = %e, path = %path.display(), "could not chmod admin db");
    }
}

#[cfg(not(unix))]
fn tighten_db_perms(_path: &Path) {
    // Non-Unix targets (Windows) don't have a meaningful equivalent.
    // The admin panel on Windows should be locked down via NTFS ACLs
    // out-of-band; we deliberately don't try to emulate that here.
}

/// BLAKE3 of the session token, hex-encoded.
///
/// Used as the lookup key in the `sessions` table. The cookie carries
/// the raw 32-hex-char token; we hash on every insert / lookup / delete
/// so an attacker with read access to `admin.db` only ever sees the
/// preimage-resistant hash, not anything they can present back over
/// the wire.
///
/// BLAKE3 is unkeyed and the input is already 128 bits of CSPRNG
/// output, so a salt would add nothing — and a deterministic hash is
/// the whole point (we *need* the same input to map to the same row).
pub fn hash_session_token(raw: &str) -> String {
    let h = blake3::hash(raw.as_bytes());
    h.to_hex().to_string()
}

/// Seconds since the unix epoch, as `i64` so sqlite's INTEGER column
/// holds it natively. We don't need sub-second precision here — TTLs
/// are measured in hours.
pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn migration_is_idempotent() {
        // Running migrate twice on the same connection must not fail
        // and must not duplicate the schema (CREATE TABLE IF NOT EXISTS
        // is the only thing guaranteeing this — assert it explicitly).
        let db = AdminDb::open_in_memory().unwrap();
        db.migrate().await.unwrap();
        db.migrate().await.unwrap();
        assert_eq!(db.user_count().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn user_insert_and_lookup_round_trip() {
        let db = AdminDb::open_in_memory().unwrap();
        db.migrate().await.unwrap();
        db.insert_user("admin", "$argon2id$fake$hash")
            .await
            .unwrap();
        assert_eq!(db.user_count().await.unwrap(), 1);
        let hash = db.get_password_hash("admin").await.unwrap().unwrap();
        assert_eq!(hash, "$argon2id$fake$hash");
        // Unknown user must surface as None, not an error.
        assert!(db.get_password_hash("ghost").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn session_round_trip_and_expiry() {
        let db = AdminDb::open_in_memory().unwrap();
        db.migrate().await.unwrap();
        db.insert_user("admin", "h").await.unwrap();

        // Active session looks up successfully.
        let future = now_unix() + 3600;
        db.create_session("tok-valid", "admin", future)
            .await
            .unwrap();
        let row = db.lookup_session("tok-valid").await.unwrap().unwrap();
        assert_eq!(row.username, "admin");
        assert_eq!(row.expires_at, future);

        // Expired session looks up as None (same as unknown).
        db.create_session("tok-expired", "admin", now_unix() - 10)
            .await
            .unwrap();
        assert!(db.lookup_session("tok-expired").await.unwrap().is_none());
        assert!(db.lookup_session("tok-ghost").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn raw_token_never_lands_on_disk() {
        // H2: the cookie value must not appear in the sqlite file —
        // only its BLAKE3 hash should. Smoke-test by inserting a
        // recognisable raw token, dumping the row, and asserting
        // the raw bytes aren't present anywhere in the column.
        let db = AdminDb::open_in_memory().unwrap();
        db.migrate().await.unwrap();
        db.insert_user("admin", "h").await.unwrap();
        let raw = "deadbeef-this-is-the-cookie-value";
        db.create_session(raw, "admin", now_unix() + 60)
            .await
            .unwrap();
        let stored: String = db
            .with_conn(|c| c.query_row("SELECT token_hash FROM sessions LIMIT 1", [], |r| r.get(0)))
            .await
            .unwrap();
        assert!(
            !stored.contains(raw),
            "raw token leaked into token_hash column",
        );
        assert_eq!(
            stored.len(),
            64,
            "expected 64-char BLAKE3 hex; got {}",
            stored.len(),
        );
        // Round-trip via lookup still works.
        let row = db.lookup_session(raw).await.unwrap().unwrap();
        assert_eq!(row.username, "admin");
    }

    #[tokio::test]
    async fn delete_session_is_idempotent() {
        let db = AdminDb::open_in_memory().unwrap();
        db.migrate().await.unwrap();
        db.insert_user("admin", "h").await.unwrap();
        db.create_session("tok", "admin", now_unix() + 60)
            .await
            .unwrap();
        db.delete_session("tok").await.unwrap();
        // Second delete on the same token is fine.
        db.delete_session("tok").await.unwrap();
        // And a delete on an unknown token is also fine.
        db.delete_session("never-existed").await.unwrap();
    }

    #[tokio::test]
    async fn update_password_hash_overwrites_existing_row() {
        let db = AdminDb::open_in_memory().unwrap();
        db.migrate().await.unwrap();
        db.insert_user("admin", "first-hash").await.unwrap();
        db.update_password_hash("admin", "second-hash")
            .await
            .unwrap();
        let got = db.get_password_hash("admin").await.unwrap().unwrap();
        assert_eq!(got, "second-hash");
    }

    #[tokio::test]
    async fn delete_other_sessions_for_user_keeps_current() {
        // Three concurrent sessions for "admin". After deleting "the
        // others", only the one matching keep_token_hash survives.
        let db = AdminDb::open_in_memory().unwrap();
        db.migrate().await.unwrap();
        db.insert_user("admin", "h").await.unwrap();
        let future = now_unix() + 3600;
        db.create_session("keep-me", "admin", future).await.unwrap();
        db.create_session("kill-1", "admin", future).await.unwrap();
        db.create_session("kill-2", "admin", future).await.unwrap();
        let keep_hash = hash_session_token("keep-me");
        let n = db
            .delete_other_sessions_for_user("admin", &keep_hash)
            .await
            .unwrap();
        assert_eq!(n, 2);
        assert!(db.lookup_session("keep-me").await.unwrap().is_some());
        assert!(db.lookup_session("kill-1").await.unwrap().is_none());
        assert!(db.lookup_session("kill-2").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn server_config_loads_defaults_on_fresh_db() {
        // After a fresh migration, the INSERT OR IGNORE seeds the
        // row with defaults from the column DEFAULT clauses. Load
        // must return those defaults verbatim — they're the same
        // values the legacy hardcoded constants used.
        let db = AdminDb::open_in_memory().unwrap();
        db.migrate().await.unwrap();
        let cfg = db.load_server_config().await.unwrap();
        assert_eq!(cfg.server_name, "");
        assert_eq!(cfg.max_peers, 256);
        assert_eq!(cfg.idle_kick_secs, 10);
    }

    #[tokio::test]
    async fn server_config_round_trips() {
        let db = AdminDb::open_in_memory().unwrap();
        db.migrate().await.unwrap();
        let new = ServerConfig {
            server_name: "Singular Toki".into(),
            max_peers: 1024,
            idle_kick_secs: 30,
            grpc_password: "hunter2".into(),
            named_channels_enabled: true,
        };
        db.save_server_config(&new).await.unwrap();
        let loaded = db.load_server_config().await.unwrap();
        assert_eq!(loaded.server_name, "Singular Toki");
        assert_eq!(loaded.max_peers, 1024);
        assert_eq!(loaded.idle_kick_secs, 30);
        assert_eq!(loaded.grpc_password, "hunter2");
        assert!(loaded.named_channels_enabled);
    }

    #[tokio::test]
    async fn migrate_adds_grpc_password_to_pre_existing_table() {
        // Upgrade path: a db created by a binary that pre-dates the
        // grpc_password column should grow that column on next
        // migrate(). Simulate by manually creating the old shape,
        // then running migrate(), then verifying the column is
        // present and load/save round-trip it.
        let db = AdminDb::open_in_memory().unwrap();
        db.with_conn(|c| {
            c.execute_batch(
                r#"
                CREATE TABLE server_config (
                    id              INTEGER PRIMARY KEY CHECK (id = 1),
                    server_name     TEXT    NOT NULL DEFAULT '',
                    max_peers       INTEGER NOT NULL DEFAULT 256,
                    idle_kick_secs  INTEGER NOT NULL DEFAULT 10,
                    updated_at      INTEGER NOT NULL DEFAULT 0
                );
                INSERT INTO server_config (id) VALUES (1);
                CREATE TABLE admin_users (
                    username TEXT PRIMARY KEY NOT NULL,
                    password_hash TEXT NOT NULL,
                    created_at INTEGER NOT NULL
                );
                CREATE TABLE sessions (
                    token_hash TEXT PRIMARY KEY NOT NULL,
                    username TEXT NOT NULL,
                    expires_at INTEGER NOT NULL
                );
                "#,
            )?;
            Ok(())
        })
        .await
        .unwrap();
        // Now run the migration the upgrade path would take.
        db.migrate().await.unwrap();
        // The new column should be present and the row should
        // round-trip through load/save with grpc_password set.
        db.save_server_config(&ServerConfig {
            server_name: "after-upgrade".into(),
            max_peers: 128,
            idle_kick_secs: 7,
            grpc_password: "secret".into(),
            named_channels_enabled: true,
        })
        .await
        .unwrap();
        let loaded = db.load_server_config().await.unwrap();
        assert_eq!(loaded.grpc_password, "secret");
        // The named_channels_enabled column was added by the same
        // upgrade path and round-trips too.
        assert!(loaded.named_channels_enabled);
        // Running migrate() again must remain idempotent (no error,
        // no second ALTER fail).
        db.migrate().await.unwrap();
    }

    #[tokio::test]
    async fn server_config_is_singleton() {
        // The CHECK (id = 1) constraint should refuse any attempt
        // to add a second row. Smoke this so a future "let's relax
        // the schema" change can't slip through unnoticed.
        let db = AdminDb::open_in_memory().unwrap();
        db.migrate().await.unwrap();
        let err = db
            .with_conn(|c| {
                c.execute(
                    "INSERT INTO server_config (id, server_name) VALUES (2, 'rogue')",
                    [],
                )
            })
            .await;
        assert!(err.is_err(), "second row should be rejected by CHECK");
    }

    #[tokio::test]
    async fn channel_names_crud_roundtrips() {
        let db = AdminDb::open_in_memory().unwrap();
        db.migrate().await.unwrap();
        assert!(db.load_channel_names().await.unwrap().is_empty());

        db.set_channel_name("446.05", "Ops Net").await.unwrap();
        db.set_channel_name("447.00", "Backup").await.unwrap();
        // Upsert: re-setting the same freq replaces, doesn't duplicate.
        db.set_channel_name("446.05", "Dispatch").await.unwrap();
        let names = db.load_channel_names().await.unwrap();
        assert_eq!(names.len(), 2);
        assert_eq!(names.get("446.05").map(String::as_str), Some("Dispatch"));
        assert_eq!(names.get("447.00").map(String::as_str), Some("Backup"));

        db.clear_channel_name("446.05").await.unwrap();
        let names = db.load_channel_names().await.unwrap();
        assert_eq!(names.len(), 1);
        assert!(!names.contains_key("446.05"));

        db.clear_all_channel_names().await.unwrap();
        assert!(db.load_channel_names().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn metrics_insert_load_prune() {
        let db = AdminDb::open_in_memory().unwrap();
        db.migrate().await.unwrap();
        for ts in [100, 200, 300] {
            db.insert_metric_sample(ts, ts as u64, (ts / 2) as u64, 3, 1)
                .await
                .unwrap();
        }
        let all = db.load_metrics(0).await.unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].ts, 100); // oldest-first
        assert_eq!(all[2].rx_bps, 300);
        // since-filter
        assert_eq!(db.load_metrics(250).await.unwrap().len(), 1);
        // prune drops rows strictly older than the cutoff
        db.prune_metrics(200).await.unwrap();
        let kept = db.load_metrics(0).await.unwrap();
        assert_eq!(kept.len(), 2);
        assert_eq!(kept[0].ts, 200);
    }

    #[tokio::test]
    async fn audit_insert_load_filter_page_prune() {
        let db = AdminDb::open_in_memory().unwrap();
        db.migrate().await.unwrap();
        db.insert_audit(10, "connect", "A", "", "").await.unwrap();
        db.insert_audit(20, "kick", "admin", "446.05", "")
            .await
            .unwrap();
        db.insert_audit(30, "auth-fail", "SYSTEM", "", "")
            .await
            .unwrap();

        // No filter (ALL), newest-first.
        let (rows, total) = db.load_audit(&[], 50, 0).await.unwrap();
        assert_eq!(total, 3);
        assert_eq!(rows[0].kind, "auth-fail");

        // Category filter.
        let (rows, total) = db.load_audit(&["kick", "rename"], 50, 0).await.unwrap();
        assert_eq!(total, 1);
        assert_eq!(rows[0].kind, "kick");

        // Paging via before_id (id of the newest row is 3).
        let newest_id = db.load_audit(&[], 1, 0).await.unwrap().0[0].id;
        let (page2, _) = db.load_audit(&[], 50, newest_id).await.unwrap();
        assert_eq!(page2.len(), 2);
        assert!(page2.iter().all(|r| r.id < newest_id));

        // Prune older than 25 → drops the two oldest.
        db.prune_audit(25).await.unwrap();
        let (rows, total) = db.load_audit(&[], 50, 0).await.unwrap();
        assert_eq!(total, 1);
        assert_eq!(rows[0].kind, "auth-fail");
    }

    #[cfg(unix)]
    #[test]
    fn open_chmods_db_to_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("toki-db-perms-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("admin.db");
        let _db = AdminDb::open(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(mode, 0o600, "admin.db must be chmod 0600, was {mode:o}");
    }

    #[tokio::test]
    async fn prune_drops_only_expired_rows() {
        let db = AdminDb::open_in_memory().unwrap();
        db.migrate().await.unwrap();
        db.insert_user("admin", "h").await.unwrap();
        db.create_session("active", "admin", now_unix() + 60)
            .await
            .unwrap();
        db.create_session("stale", "admin", now_unix() - 60)
            .await
            .unwrap();
        let pruned = db.prune_expired_sessions().await.unwrap();
        assert_eq!(pruned, 1);
        assert!(db.lookup_session("active").await.unwrap().is_some());
        assert!(db.lookup_session("stale").await.unwrap().is_none());
    }
}
