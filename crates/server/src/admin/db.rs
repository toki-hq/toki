//! Admin store — multi-backend (SQLite / MariaDB-MySQL / PostgreSQL).
//!
//! Holds admin users, sessions, runtime server config, channel names,
//! metrics samples, and the audit log. The backend is chosen by the
//! connection URL passed to [`AdminDb::open`]:
//!
//! * `sqlite://<path>?mode=rwc` — embedded, zero-config default. The
//!   driver vendors libsqlite3 statically, so the binary stays
//!   self-contained, and the file is `chmod 0600` (it holds argon2
//!   hashes + session-token hashes).
//! * `mysql://…` / `mariadb://…` — a remote MariaDB/MySQL server.
//! * `postgres://…` — a remote PostgreSQL server.
//!
//! Built on SQLx (async-native; no `spawn_blocking`). All access goes
//! through a per-backend [`Pool`] enum; the [`on_pool!`] macro expands
//! each method body once per concrete pool type, so we get each driver's
//! real type map without generic-over-`Row` gymnastics. SQL is shared
//! across dialects except for placeholder style (`?` vs `$1`, handled by
//! [`AdminDb::q`]) and the few upserts/DDL statements that genuinely
//! differ.
//!
//! Security note: the `sessions` table stores `BLAKE3(token)` (64-char
//! hex), never the raw cookie value — see [`hash_session_token`].
//!
//! Multi-backend is **fresh-start**: switching to MariaDB/Postgres
//! creates empty tables and re-seeds the admin user; no data is copied
//! from an existing SQLite file.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use sqlx::mysql::MySqlPoolOptions;
use sqlx::postgres::PgPoolOptions;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{MySqlPool, PgPool, Row, SqlitePool};

use crate::server_config::ServerConfig;
use crate::state::IdentityRecord;

/// Which SQL dialect the open connection speaks.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Backend {
    Sqlite,
    MySql,
    Postgres,
}

/// The live connection pool, one variant per backend. `Clone` is a cheap
/// `Arc` bump (sqlx pools are internally `Arc`), matching the old
/// "every handler clones `AppState`" model.
#[derive(Clone)]
enum Pool {
    Sqlite(SqlitePool),
    MySql(MySqlPool),
    Postgres(PgPool),
}

/// Async, multi-backend admin store. Public method signatures are
/// identical across backends, so callers (`auth`, `grpc`, `handlers`,
/// `audit`, `metrics`, `mod`) are backend-agnostic.
#[derive(Clone)]
pub struct AdminDb {
    pool: Pool,
    backend: Backend,
}

/// Run a method body once per concrete pool type. The body is expanded
/// textually in each match arm with `$p` bound to the concrete pool, so
/// every `sqlx::query`/`Row::try_get` monomorphizes against that driver
/// — no generic bounds needed. Only the matching arm runs at runtime.
macro_rules! on_pool {
    ($self:expr, $p:ident, $body:block) => {
        match &$self.pool {
            Pool::Sqlite($p) => $body,
            Pool::MySql($p) => $body,
            Pool::Postgres($p) => $body,
        }
    };
}

impl AdminDb {
    /// Open (and pool) the admin store from a connection URL. Does *not*
    /// run migrations — call [`migrate`](Self::migrate) explicitly so
    /// callers fail fast on schema errors.
    ///
    /// For a file-backed SQLite URL the parent dir is created and the
    /// file is `chmod 0600` after open (it holds password + session
    /// hashes). SQLite pools are capped at one connection (single-writer;
    /// also makes `sqlite::memory:` test pools share one database).
    /// MySQL/Postgres connect over TLS when the server requires it
    /// (rustls/ring).
    pub async fn open(database_url: &str) -> Result<Self> {
        let backend = detect_backend(database_url)?;
        let pool = match backend {
            Backend::Sqlite => {
                let opts = SqliteConnectOptions::from_str(database_url)
                    .with_context(|| format!("parse sqlite url {database_url}"))?
                    .create_if_missing(true);
                let file = sqlite_file_path(database_url, &opts);
                if let Some(parent) = file.as_ref().and_then(|p| p.parent()) {
                    if !parent.as_os_str().is_empty() {
                        std::fs::create_dir_all(parent).with_context(|| {
                            format!("create admin db parent {}", parent.display())
                        })?;
                    }
                }
                let pool = SqlitePoolOptions::new()
                    .max_connections(1)
                    .connect_with(opts)
                    .await
                    .with_context(|| format!("open sqlite {database_url}"))?;
                if let Some(path) = &file {
                    tighten_db_perms(path);
                }
                Pool::Sqlite(pool)
            }
            Backend::MySql => {
                // sqlx's MySQL driver expects a `mysql://` scheme; map
                // the `mariadb://` alias onto it.
                let url = database_url.replacen("mariadb://", "mysql://", 1);
                // Retry with backoff so a DB container that's still
                // starting (docker-compose / k8s ordering) gets time to
                // accept connections instead of failing the boot.
                let pool = connect_with_retry("MariaDB/MySQL", || {
                    MySqlPoolOptions::new().max_connections(5).connect(&url)
                })
                .await?;
                Pool::MySql(pool)
            }
            Backend::Postgres => {
                let pool = connect_with_retry("PostgreSQL", || {
                    PgPoolOptions::new()
                        .max_connections(5)
                        .connect(database_url)
                })
                .await?;
                Pool::Postgres(pool)
            }
        };
        Ok(Self { pool, backend })
    }

    /// Convenience for tests: an in-memory SQLite store.
    pub async fn open_in_memory() -> Result<Self> {
        Self::open("sqlite::memory:").await
    }

    /// Human label for the active backend (for startup logging).
    pub fn backend_label(&self) -> &'static str {
        match self.backend {
            Backend::Sqlite => "sqlite",
            Backend::MySql => "mysql/mariadb",
            Backend::Postgres => "postgres",
        }
    }

    /// Rewrite `?` placeholders to `$1..$N` for Postgres; pass through
    /// for SQLite/MySQL. Applied to every statement that binds params.
    fn q(&self, sql: &str) -> String {
        if self.backend == Backend::Postgres {
            pg_rewrite(sql)
        } else {
            sql.to_string()
        }
    }

    /// Apply the schema. Idempotent (`CREATE TABLE IF NOT EXISTS`). Each
    /// backend gets its own DDL block (autoincrement / text-PK / index
    /// syntax differ).
    ///
    /// `CREATE TABLE IF NOT EXISTS` is a **no-op against a table that
    /// already exists**, so a column added to the DDL in a later release
    /// never lands on a database created by an older build. Every backend
    /// therefore runs an additive `ALTER TABLE ADD COLUMN` upgrade pass
    /// for the columns that postdate the original `server_config` shape
    /// (`grpc_password`, `named_channels_enabled`, `audio_quality`).
    /// Each add is guarded so it's a no-op when the column is already
    /// present — making the whole `migrate()` idempotent across fresh
    /// installs *and* in-place upgrades on all three backends.
    pub async fn migrate(&self) -> Result<()> {
        // Columns added after the original `server_config` baseline
        // (`id, server_name, max_peers, idle_kick_secs, updated_at`),
        // each with its SQL type spec per dialect. Adding a new mutable
        // setting means appending one row here (plus the DDL + struct).
        // `(column, sqlite_spec, pg_spec, mysql_spec)`.
        const SERVER_CONFIG_ADDED_COLUMNS: &[(&str, &str, &str, &str)] = &[
            (
                "grpc_password",
                "TEXT NOT NULL DEFAULT ''",
                "TEXT NOT NULL DEFAULT ''",
                "VARCHAR(255) NOT NULL DEFAULT ''",
            ),
            (
                "named_channels_enabled",
                "INTEGER NOT NULL DEFAULT 0",
                "BIGINT NOT NULL DEFAULT 0",
                "BIGINT NOT NULL DEFAULT 0",
            ),
            (
                "audio_quality",
                "INTEGER NOT NULL DEFAULT 2",
                "BIGINT NOT NULL DEFAULT 2",
                "BIGINT NOT NULL DEFAULT 2",
            ),
        ];
        // Columns that existed in pre-release dev builds and were later
        // removed from the schema. A `NOT NULL` stray blocks inserts
        // that no longer supply it, so migrate() drops them when found.
        // No-op on any database that never had them. `(table, column)`.
        const DROPPED_COLUMNS: &[(&str, &str)] = &[
            // Identity display ids were briefly callsign-prefixed; the
            // prefix column was removed before the 0.5.0 release.
            ("identities", "first_callsign"),
        ];
        match &self.pool {
            Pool::Sqlite(p) => {
                for stmt in split_ddl(SQLITE_DDL) {
                    sqlx::query(stmt)
                        .execute(p)
                        .await
                        .with_context(|| format!("sqlite ddl: {stmt}"))?;
                }
                // Upgrade pre-existing files that predate later columns
                // (CREATE TABLE IF NOT EXISTS won't add them).
                for (column, spec, _, _) in SERVER_CONFIG_ADDED_COLUMNS {
                    ensure_column_sqlite(p, "server_config", column, spec).await?;
                }
                for (table, column) in DROPPED_COLUMNS {
                    drop_column_sqlite(p, table, column).await?;
                }
            }
            Pool::MySql(p) => {
                for stmt in split_ddl(MYSQL_DDL) {
                    sqlx::query(stmt)
                        .execute(p)
                        .await
                        .with_context(|| format!("mysql ddl: {stmt}"))?;
                }
                // Upgrade pre-existing schemas that predate later columns.
                for (column, _, _, spec) in SERVER_CONFIG_ADDED_COLUMNS {
                    ensure_column_mysql(p, "server_config", column, spec).await?;
                }
                for (table, column) in DROPPED_COLUMNS {
                    drop_column_mysql(p, table, column).await?;
                }
            }
            Pool::Postgres(p) => {
                for stmt in split_ddl(POSTGRES_DDL) {
                    sqlx::query(stmt)
                        .execute(p)
                        .await
                        .with_context(|| format!("postgres ddl: {stmt}"))?;
                }
                // Upgrade pre-existing schemas that predate later columns.
                for (column, _, spec, _) in SERVER_CONFIG_ADDED_COLUMNS {
                    ensure_column_pg(p, "server_config", column, spec).await?;
                }
                for (table, column) in DROPPED_COLUMNS {
                    sqlx::query(&format!(
                        "ALTER TABLE {table} DROP COLUMN IF EXISTS {column}"
                    ))
                    .execute(p)
                    .await
                    .with_context(|| format!("drop {table}.{column} (pg)"))?;
                }
            }
        }
        Ok(())
    }

    // ── Server config (singleton row id = 1) ──────────────────────────

    /// Read the singleton `server_config` row, or `Default` if absent.
    pub async fn load_server_config(&self) -> Result<ServerConfig> {
        let sql = "SELECT server_name, max_peers, idle_kick_secs, grpc_password, \
                   named_channels_enabled, audio_quality \
                   FROM server_config WHERE id = 1";
        on_pool!(self, p, {
            let row = sqlx::query(sql).fetch_optional(p).await?;
            Ok(match row {
                Some(r) => ServerConfig {
                    server_name: r.try_get::<String, _>(0)?,
                    max_peers: r.try_get::<i64, _>(1)? as u32,
                    idle_kick_secs: r.try_get::<i64, _>(2)? as u32,
                    grpc_password: r.try_get::<String, _>(3)?,
                    named_channels_enabled: r.try_get::<i64, _>(4)? != 0,
                    audio_quality: r.try_get::<i64, _>(5)? as u32,
                },
                None => ServerConfig::default(),
            })
        })
    }

    /// Persist the singleton config, stamping `updated_at` to now.
    pub async fn save_server_config(&self, cfg: &ServerConfig) -> Result<()> {
        let now = now_unix();
        let sql = self.q("UPDATE server_config \
             SET server_name = ?, max_peers = ?, idle_kick_secs = ?, grpc_password = ?, \
                 named_channels_enabled = ?, audio_quality = ?, updated_at = ? \
             WHERE id = 1");
        on_pool!(self, p, {
            sqlx::query(&sql)
                .bind(cfg.server_name.as_str())
                .bind(cfg.max_peers as i64)
                .bind(cfg.idle_kick_secs as i64)
                .bind(cfg.grpc_password.as_str())
                .bind(cfg.named_channels_enabled as i64)
                .bind(cfg.audio_quality as i64)
                .bind(now)
                .execute(p)
                .await?;
            Ok(())
        })
    }

    // ── Channel names ─────────────────────────────────────────────────

    /// Load every channel name into a `frequency → name` map.
    pub async fn load_channel_names(&self) -> Result<HashMap<String, String>> {
        on_pool!(self, p, {
            let rows = sqlx::query("SELECT frequency, name FROM channel_names")
                .fetch_all(p)
                .await?;
            let mut map = HashMap::with_capacity(rows.len());
            for r in &rows {
                map.insert(r.try_get::<String, _>(0)?, r.try_get::<String, _>(1)?);
            }
            Ok(map)
        })
    }

    /// Upsert a single channel name (caller validated freq + ≤16-char name).
    pub async fn set_channel_name(&self, frequency: &str, name: &str) -> Result<()> {
        let now = now_unix();
        let sql = self.channel_upsert_sql();
        on_pool!(self, p, {
            sqlx::query(sql)
                .bind(frequency)
                .bind(name)
                .bind(now)
                .execute(p)
                .await?;
            Ok(())
        })
    }

    /// Delete a single channel name (no-op if absent).
    pub async fn clear_channel_name(&self, frequency: &str) -> Result<()> {
        let sql = self.q("DELETE FROM channel_names WHERE frequency = ?");
        on_pool!(self, p, {
            sqlx::query(&sql).bind(frequency).execute(p).await?;
            Ok(())
        })
    }

    /// Delete every channel name.
    pub async fn clear_all_channel_names(&self) -> Result<()> {
        on_pool!(self, p, {
            sqlx::query("DELETE FROM channel_names").execute(p).await?;
            Ok(())
        })
    }

    // ── Client identities ─────────────────────────────────────────────

    /// Load every known identity into a `pubkey-hex → record` map —
    /// the boot-time hydration of `SharedIdentities`.
    pub async fn load_identities(&self) -> Result<HashMap<String, IdentityRecord>> {
        let sql = "SELECT pubkey, display_id, last_callsign, machine_hash, \
                   origin_client_id, first_seen, last_seen, last_ip FROM identities";
        on_pool!(self, p, {
            let rows = sqlx::query(sql).fetch_all(p).await?;
            let mut map = HashMap::with_capacity(rows.len());
            for r in &rows {
                map.insert(
                    r.try_get::<String, _>(0)?,
                    IdentityRecord {
                        display_id: r.try_get::<String, _>(1)?,
                        last_callsign: r.try_get::<String, _>(2)?,
                        machine_hash: r.try_get::<String, _>(3)?,
                        origin_client_id: r.try_get::<String, _>(4)?,
                        first_seen: r.try_get::<i64, _>(5)?,
                        last_seen: r.try_get::<i64, _>(6)?,
                        last_ip: r.try_get::<String, _>(7)?,
                    },
                );
            }
            Ok(map)
        })
    }

    /// Upsert one identity record. On conflict the *immutable* facts
    /// keep their stored values — `display_id` and `first_seen` never
    /// change once written, and `origin_client_id` is
    /// first-non-empty-wins — so a record fed through a boot race
    /// (register before the hydration finished) can't rewrite an
    /// identity's history. The mutable last-* columns track the most
    /// recent register.
    pub async fn upsert_identity(&self, pubkey: &str, rec: &IdentityRecord) -> Result<()> {
        let sql = self.identity_upsert_sql();
        on_pool!(self, p, {
            sqlx::query(sql)
                .bind(pubkey)
                .bind(rec.display_id.as_str())
                .bind(rec.last_callsign.as_str())
                .bind(rec.machine_hash.as_str())
                .bind(rec.origin_client_id.as_str())
                .bind(rec.first_seen)
                .bind(rec.last_seen)
                .bind(rec.last_ip.as_str())
                .execute(p)
                .await?;
            Ok(())
        })
    }

    // ── Metrics time-series ───────────────────────────────────────────

    /// Append one metrics sample (1-minute cadence). Upserts on the `ts`
    /// PK so two ticks in the same second don't collide.
    pub async fn insert_metric_sample(
        &self,
        ts: i64,
        rx_bps: u64,
        tx_bps: u64,
        users: u32,
        transmitting: u32,
    ) -> Result<()> {
        let sql = self.metrics_upsert_sql();
        on_pool!(self, p, {
            sqlx::query(sql)
                .bind(ts)
                .bind(rx_bps as i64)
                .bind(tx_bps as i64)
                .bind(users as i64)
                .bind(transmitting as i64)
                .execute(p)
                .await?;
            Ok(())
        })
    }

    /// Load samples with `ts >= since`, oldest-first.
    pub async fn load_metrics(&self, since: i64) -> Result<Vec<MetricRow>> {
        let sql = self.q(
            "SELECT ts, rx_bps, tx_bps, users, transmitting FROM metrics_samples \
             WHERE ts >= ? ORDER BY ts ASC",
        );
        on_pool!(self, p, {
            let rows = sqlx::query(&sql).bind(since).fetch_all(p).await?;
            let mut out = Vec::with_capacity(rows.len());
            for r in &rows {
                out.push(MetricRow {
                    ts: r.try_get::<i64, _>(0)?,
                    rx_bps: r.try_get::<i64, _>(1)? as u64,
                    tx_bps: r.try_get::<i64, _>(2)? as u64,
                    users: r.try_get::<i64, _>(3)? as u32,
                    transmitting: r.try_get::<i64, _>(4)? as u32,
                });
            }
            Ok(out)
        })
    }

    /// Delete metrics rows older than `cutoff` (unix seconds).
    pub async fn prune_metrics(&self, cutoff: i64) -> Result<()> {
        let sql = self.q("DELETE FROM metrics_samples WHERE ts < ?");
        on_pool!(self, p, {
            sqlx::query(&sql).bind(cutoff).execute(p).await?;
            Ok(())
        })
    }

    // ── Audit log ─────────────────────────────────────────────────────

    /// Append an audit entry (caller supplies the unix timestamp).
    pub async fn insert_audit(
        &self,
        ts: i64,
        kind: &str,
        actor: &str,
        frequency: &str,
        detail: &str,
    ) -> Result<()> {
        let sql = self.q(
            "INSERT INTO audit_log (ts, kind, actor, frequency, detail) \
             VALUES (?, ?, ?, ?, ?)",
        );
        on_pool!(self, p, {
            sqlx::query(&sql)
                .bind(ts)
                .bind(kind)
                .bind(actor)
                .bind(frequency)
                .bind(detail)
                .execute(p)
                .await?;
            Ok(())
        })
    }

    /// Page the audit log newest-first. `kinds` is the allowed-kind set
    /// for the active filter (empty = ALL); `before_id` of 0 = newest
    /// page, else rows with `id < before_id`. Returns `(rows, total)`.
    ///
    /// The `kind`/`before_id`/`limit` values are code-supplied (clamped,
    /// fixed vocabulary — never user input), so they're interpolated;
    /// no bound params, hence portable verbatim across backends.
    pub async fn load_audit(
        &self,
        kinds: &[&str],
        limit: u32,
        before_id: u64,
    ) -> Result<(Vec<AuditRow>, u64)> {
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
        let count_sql = format!("SELECT COUNT(*) FROM audit_log WHERE 1=1{kind_clause}");
        let sql = format!(
            "SELECT id, ts, kind, actor, frequency, detail FROM audit_log \
             WHERE 1=1{kind_clause}{before_clause} ORDER BY id DESC LIMIT {limit}"
        );
        on_pool!(self, p, {
            let total: i64 = sqlx::query(&count_sql).fetch_one(p).await?.try_get(0)?;
            let rows = sqlx::query(&sql).fetch_all(p).await?;
            let mut out = Vec::with_capacity(rows.len());
            for r in &rows {
                out.push(AuditRow {
                    id: r.try_get::<i64, _>(0)? as u64,
                    ts: r.try_get::<i64, _>(1)?,
                    kind: r.try_get::<String, _>(2)?,
                    actor: r.try_get::<String, _>(3)?,
                    frequency: r.try_get::<String, _>(4)?,
                    detail: r.try_get::<String, _>(5)?,
                });
            }
            Ok((out, total as u64))
        })
    }

    /// Delete audit rows older than `cutoff` (unix seconds).
    pub async fn prune_audit(&self, cutoff: i64) -> Result<()> {
        let sql = self.q("DELETE FROM audit_log WHERE ts < ?");
        on_pool!(self, p, {
            sqlx::query(&sql).bind(cutoff).execute(p).await?;
            Ok(())
        })
    }

    // ── Admin users + auth ─────────────────────────────────────────────

    /// Count rows in `admin_users` (the seeder gate).
    pub async fn user_count(&self) -> Result<i64> {
        on_pool!(self, p, {
            let n: i64 = sqlx::query("SELECT COUNT(*) FROM admin_users")
                .fetch_one(p)
                .await?
                .try_get(0)?;
            Ok(n)
        })
    }

    /// Insert a new admin user. Errors on a duplicate username.
    pub async fn insert_user(&self, username: &str, password_hash: &str) -> Result<()> {
        let now = now_unix();
        let sql = self
            .q("INSERT INTO admin_users (username, password_hash, created_at) VALUES (?, ?, ?)");
        on_pool!(self, p, {
            sqlx::query(&sql)
                .bind(username)
                .bind(password_hash)
                .bind(now)
                .execute(p)
                .await?;
            Ok(())
        })
    }

    /// Replace the stored argon2 hash for an existing user.
    pub async fn update_password_hash(&self, username: &str, new_hash: &str) -> Result<()> {
        let sql = self.q("UPDATE admin_users SET password_hash = ? WHERE username = ?");
        on_pool!(self, p, {
            sqlx::query(&sql)
                .bind(new_hash)
                .bind(username)
                .execute(p)
                .await?;
            Ok(())
        })
    }

    /// Delete every session for `username` except the one whose hash is
    /// `keep_token_hash`. Returns the number removed (used after a
    /// password change to kill other sessions).
    pub async fn delete_other_sessions_for_user(
        &self,
        username: &str,
        keep_token_hash: &str,
    ) -> Result<u64> {
        let sql = self.q("DELETE FROM sessions WHERE username = ? AND token_hash != ?");
        on_pool!(self, p, {
            let r = sqlx::query(&sql)
                .bind(username)
                .bind(keep_token_hash)
                .execute(p)
                .await?;
            Ok(r.rows_affected())
        })
    }

    /// Look up the password hash for a username (`None` if no such user).
    pub async fn get_password_hash(&self, username: &str) -> Result<Option<String>> {
        let sql = self.q("SELECT password_hash FROM admin_users WHERE username = ?");
        on_pool!(self, p, {
            let row = sqlx::query(&sql).bind(username).fetch_optional(p).await?;
            Ok(row.map(|r| r.try_get::<String, _>(0)).transpose()?)
        })
    }

    // ── Sessions ───────────────────────────────────────────────────────

    /// Insert a fresh session. The cookie value is `token`; only
    /// `BLAKE3(token)` is stored.
    pub async fn create_session(&self, token: &str, username: &str, expires_at: i64) -> Result<()> {
        let token_hash = hash_session_token(token);
        let sql =
            self.q("INSERT INTO sessions (token_hash, username, expires_at) VALUES (?, ?, ?)");
        on_pool!(self, p, {
            sqlx::query(&sql)
                .bind(token_hash.as_str())
                .bind(username)
                .bind(expires_at)
                .execute(p)
                .await?;
            Ok(())
        })
    }

    /// Resolve a cookie token to `(username, expiry)`. `None` if unknown
    /// or expired. Hashes the token before the query.
    pub async fn lookup_session(&self, token: &str) -> Result<Option<SessionRow>> {
        let token_hash = hash_session_token(token);
        let now = now_unix();
        let sql = self
            .q("SELECT username, expires_at FROM sessions WHERE token_hash = ? AND expires_at > ?");
        on_pool!(self, p, {
            let row = sqlx::query(&sql)
                .bind(token_hash.as_str())
                .bind(now)
                .fetch_optional(p)
                .await?;
            Ok(match row {
                Some(r) => Some(SessionRow {
                    username: r.try_get::<String, _>(0)?,
                    expires_at: r.try_get::<i64, _>(1)?,
                }),
                None => None,
            })
        })
    }

    /// Drop a single session (idempotent). Hashes the token first.
    pub async fn delete_session(&self, token: &str) -> Result<()> {
        let token_hash = hash_session_token(token);
        let sql = self.q("DELETE FROM sessions WHERE token_hash = ?");
        on_pool!(self, p, {
            sqlx::query(&sql)
                .bind(token_hash.as_str())
                .execute(p)
                .await?;
            Ok(())
        })
    }

    /// Sweep expired sessions. Returns the number removed.
    pub async fn prune_expired_sessions(&self) -> Result<u64> {
        let now = now_unix();
        let sql = self.q("DELETE FROM sessions WHERE expires_at <= ?");
        on_pool!(self, p, {
            let r = sqlx::query(&sql).bind(now).execute(p).await?;
            Ok(r.rows_affected())
        })
    }

    // ── Per-dialect upsert SQL (the statements that genuinely differ) ──

    fn channel_upsert_sql(&self) -> &'static str {
        match self.backend {
            Backend::Sqlite => {
                "INSERT INTO channel_names (frequency, name, updated_at) VALUES (?, ?, ?) \
                 ON CONFLICT(frequency) DO UPDATE SET name = excluded.name, \
                 updated_at = excluded.updated_at"
            }
            Backend::MySql => {
                "INSERT INTO channel_names (frequency, name, updated_at) VALUES (?, ?, ?) \
                 ON DUPLICATE KEY UPDATE name = VALUES(name), updated_at = VALUES(updated_at)"
            }
            Backend::Postgres => {
                "INSERT INTO channel_names (frequency, name, updated_at) VALUES ($1, $2, $3) \
                 ON CONFLICT(frequency) DO UPDATE SET name = excluded.name, \
                 updated_at = excluded.updated_at"
            }
        }
    }

    /// Identity upsert. The non-updated columns on conflict are the
    /// deliberate immutability story — see [`AdminDb::upsert_identity`].
    fn identity_upsert_sql(&self) -> &'static str {
        match self.backend {
            Backend::Sqlite => {
                "INSERT INTO identities (pubkey, display_id, last_callsign, \
                 machine_hash, origin_client_id, first_seen, last_seen, last_ip) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?) \
                 ON CONFLICT(pubkey) DO UPDATE SET \
                 last_callsign = excluded.last_callsign, \
                 machine_hash = excluded.machine_hash, \
                 origin_client_id = CASE WHEN identities.origin_client_id = '' \
                   THEN excluded.origin_client_id ELSE identities.origin_client_id END, \
                 last_seen = excluded.last_seen, \
                 last_ip = excluded.last_ip"
            }
            Backend::MySql => {
                "INSERT INTO identities (pubkey, display_id, last_callsign, \
                 machine_hash, origin_client_id, first_seen, last_seen, last_ip) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?) \
                 ON DUPLICATE KEY UPDATE \
                 last_callsign = VALUES(last_callsign), \
                 machine_hash = VALUES(machine_hash), \
                 origin_client_id = IF(origin_client_id = '', VALUES(origin_client_id), origin_client_id), \
                 last_seen = VALUES(last_seen), \
                 last_ip = VALUES(last_ip)"
            }
            Backend::Postgres => {
                "INSERT INTO identities (pubkey, display_id, last_callsign, \
                 machine_hash, origin_client_id, first_seen, last_seen, last_ip) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
                 ON CONFLICT(pubkey) DO UPDATE SET \
                 last_callsign = excluded.last_callsign, \
                 machine_hash = excluded.machine_hash, \
                 origin_client_id = CASE WHEN identities.origin_client_id = '' \
                   THEN excluded.origin_client_id ELSE identities.origin_client_id END, \
                 last_seen = excluded.last_seen, \
                 last_ip = excluded.last_ip"
            }
        }
    }

    fn metrics_upsert_sql(&self) -> &'static str {
        match self.backend {
            Backend::Sqlite => {
                "INSERT OR REPLACE INTO metrics_samples (ts, rx_bps, tx_bps, users, transmitting) \
                 VALUES (?, ?, ?, ?, ?)"
            }
            Backend::MySql => {
                "INSERT INTO metrics_samples (ts, rx_bps, tx_bps, users, transmitting) \
                 VALUES (?, ?, ?, ?, ?) \
                 ON DUPLICATE KEY UPDATE rx_bps = VALUES(rx_bps), tx_bps = VALUES(tx_bps), \
                 users = VALUES(users), transmitting = VALUES(transmitting)"
            }
            Backend::Postgres => {
                "INSERT INTO metrics_samples (ts, rx_bps, tx_bps, users, transmitting) \
                 VALUES ($1, $2, $3, $4, $5) \
                 ON CONFLICT(ts) DO UPDATE SET rx_bps = excluded.rx_bps, tx_bps = excluded.tx_bps, \
                 users = excluded.users, transmitting = excluded.transmitting"
            }
        }
    }

    /// Raw single-statement execute, for tests that assert invariants the
    /// public API hides (e.g. the singleton CHECK).
    #[cfg(test)]
    pub(crate) async fn exec_raw(&self, sql: &str) -> Result<()> {
        on_pool!(self, p, {
            sqlx::query(sql).execute(p).await?;
            Ok(())
        })
    }

    /// Raw scalar-text fetch, for tests (e.g. reading the stored token
    /// hash to prove the raw cookie never lands on disk).
    #[cfg(test)]
    pub(crate) async fn fetch_text(&self, sql: &str) -> Result<String> {
        on_pool!(self, p, {
            Ok(sqlx::query(sql)
                .fetch_one(p)
                .await?
                .try_get::<String, _>(0)?)
        })
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

/// Total time to keep retrying the initial DB connection before giving
/// up — covers a remote DB container still starting under
/// docker-compose / k8s. Generous: most DBs accept connections within a
/// few seconds, but cold starts (volume init, replication) can take longer.
const CONNECT_RETRY_BUDGET: Duration = Duration::from_secs(60);
/// First retry delay; doubles each attempt up to [`CONNECT_MAX_DELAY`].
const CONNECT_INITIAL_DELAY: Duration = Duration::from_millis(500);
/// Cap on the per-attempt backoff delay.
const CONNECT_MAX_DELAY: Duration = Duration::from_secs(5);

/// Connect with exponential backoff. Retries transient failures (the DB
/// isn't accepting connections yet) until [`CONNECT_RETRY_BUDGET`] is
/// exhausted, then returns the last error. Only used for the remote
/// backends — a local SQLite open either works or fails for a permanent
/// reason (bad path/permissions), so retrying it would just stall boot.
async fn connect_with_retry<T, F, Fut>(label: &str, mut connect: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = std::result::Result<T, sqlx::Error>>,
{
    let start = Instant::now();
    let mut delay = CONNECT_INITIAL_DELAY;
    let mut attempt = 1u32;
    loop {
        match connect().await {
            Ok(v) => {
                if attempt > 1 {
                    tracing::info!(label, attempt, "connected to admin db after retrying");
                }
                return Ok(v);
            }
            Err(e) => {
                let elapsed = start.elapsed();
                // Stop once we'd sleep past the budget — surface the
                // real error (with context) so a genuine misconfig
                // (bad creds/host) still fails the boot promptly-ish.
                if elapsed + delay >= CONNECT_RETRY_BUDGET {
                    return Err(anyhow::Error::new(e)).with_context(|| {
                        format!(
                            "connect {label} admin db: gave up after {attempt} attempts \
                             (~{}s)",
                            elapsed.as_secs()
                        )
                    });
                }
                tracing::warn!(
                    label,
                    attempt,
                    retry_in_ms = delay.as_millis() as u64,
                    error = %e,
                    "admin db not ready; retrying"
                );
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(CONNECT_MAX_DELAY);
                attempt += 1;
            }
        }
    }
}

/// Map a connection URL's scheme to a backend.
fn detect_backend(url: &str) -> Result<Backend> {
    let scheme = url.split(':').next().unwrap_or("").to_ascii_lowercase();
    match scheme.as_str() {
        "sqlite" => Ok(Backend::Sqlite),
        "mysql" | "mariadb" => Ok(Backend::MySql),
        "postgres" | "postgresql" => Ok(Backend::Postgres),
        other => anyhow::bail!(
            "unsupported admin database URL scheme {other:?} \
             (expected sqlite / mysql / mariadb / postgres)"
        ),
    }
}

/// The on-disk path for a file-backed SQLite URL, or `None` for an
/// in-memory database (`:memory:` / `mode=memory`). Used to decide
/// whether to create the parent dir + `chmod 0600`.
fn sqlite_file_path(url: &str, opts: &SqliteConnectOptions) -> Option<PathBuf> {
    if url.contains(":memory:") || url.contains("mode=memory") {
        return None;
    }
    Some(opts.get_filename().to_path_buf())
}

/// Rewrite positional `?` placeholders to Postgres `$1..$N`. Our SQL
/// never contains a literal `?`, so a plain scan is safe.
fn pg_rewrite(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len() + 8);
    let mut n = 0u32;
    for ch in sql.chars() {
        if ch == '?' {
            n += 1;
            out.push('$');
            out.push_str(&n.to_string());
        } else {
            out.push(ch);
        }
    }
    out
}

/// Split a DDL batch into individual statements. Our DDL contains no
/// `;` inside literals or comments, so splitting on `;` is safe.
fn split_ddl(ddl: &str) -> impl Iterator<Item = &str> {
    ddl.split(';').map(str::trim).filter(|s| !s.is_empty())
}

/// Idempotently add a column to a SQLite table (SQLite has no
/// `ADD COLUMN IF NOT EXISTS`). Only ever called for the SQLite backend
/// to upgrade pre-existing files; `table`/`column`/`spec` are hardcoded.
async fn ensure_column_sqlite(
    pool: &SqlitePool,
    table: &str,
    column: &str,
    spec: &str,
) -> Result<()> {
    let rows = sqlx::query(&format!("PRAGMA table_info({table})"))
        .fetch_all(pool)
        .await
        .with_context(|| format!("pragma table_info({table})"))?;
    let exists = rows.iter().any(|r| {
        r.try_get::<String, _>("name")
            .map(|n| n == column)
            .unwrap_or(false)
    });
    if !exists {
        sqlx::query(&format!("ALTER TABLE {table} ADD COLUMN {column} {spec}"))
            .execute(pool)
            .await
            .with_context(|| format!("alter {table} add {column}"))?;
    }
    Ok(())
}

/// Idempotently add a column to a Postgres table. Postgres has supported
/// `ADD COLUMN IF NOT EXISTS` since 9.6, so a single statement is both the
/// add and the guard. `table`/`column`/`spec` are hardcoded (never user
/// input), so the format-string interpolation is safe.
async fn ensure_column_pg(pool: &PgPool, table: &str, column: &str, spec: &str) -> Result<()> {
    sqlx::query(&format!(
        "ALTER TABLE {table} ADD COLUMN IF NOT EXISTS {column} {spec}"
    ))
    .execute(pool)
    .await
    .with_context(|| format!("alter {table} add {column} (pg)"))?;
    Ok(())
}

/// Idempotently add a column to a MySQL/MariaDB table. MySQL (< 8.0.29)
/// has no `ADD COLUMN IF NOT EXISTS`, so we check `information_schema`
/// first and only `ALTER` when the column is absent — portable across
/// MariaDB and every MySQL 8 point release. `table`/`column`/`spec` are
/// hardcoded; only `table`/`column` reach the prepared lookup as binds.
async fn ensure_column_mysql(
    pool: &MySqlPool,
    table: &str,
    column: &str,
    spec: &str,
) -> Result<()> {
    let exists: i64 = sqlx::query(
        "SELECT COUNT(*) FROM information_schema.columns \
         WHERE table_schema = DATABASE() AND table_name = ? AND column_name = ?",
    )
    .bind(table)
    .bind(column)
    .fetch_one(pool)
    .await
    .with_context(|| format!("information_schema check {table}.{column}"))?
    .try_get::<i64, _>(0)?;
    if exists == 0 {
        sqlx::query(&format!("ALTER TABLE {table} ADD COLUMN {column} {spec}"))
            .execute(pool)
            .await
            .with_context(|| format!("alter {table} add {column} (mysql)"))?;
    }
    Ok(())
}

/// Idempotently drop a column from a SQLite table (SQLite has no
/// `DROP COLUMN IF EXISTS`): `PRAGMA table_info` check, then `ALTER`.
/// Heals databases created by builds whose schema briefly carried the
/// column. `table`/`column` are hardcoded.
async fn drop_column_sqlite(pool: &SqlitePool, table: &str, column: &str) -> Result<()> {
    let rows = sqlx::query(&format!("PRAGMA table_info({table})"))
        .fetch_all(pool)
        .await
        .with_context(|| format!("pragma table_info({table})"))?;
    let exists = rows.iter().any(|r| {
        r.try_get::<String, _>("name")
            .map(|n| n == column)
            .unwrap_or(false)
    });
    if exists {
        sqlx::query(&format!("ALTER TABLE {table} DROP COLUMN {column}"))
            .execute(pool)
            .await
            .with_context(|| format!("alter {table} drop {column}"))?;
    }
    Ok(())
}

/// Idempotently drop a column from a MySQL/MariaDB table — same
/// `information_schema` guard as [`ensure_column_mysql`] (MySQL has no
/// portable `DROP COLUMN IF EXISTS`).
async fn drop_column_mysql(pool: &MySqlPool, table: &str, column: &str) -> Result<()> {
    let exists: i64 = sqlx::query(
        "SELECT COUNT(*) FROM information_schema.columns \
         WHERE table_schema = DATABASE() AND table_name = ? AND column_name = ?",
    )
    .bind(table)
    .bind(column)
    .fetch_one(pool)
    .await
    .with_context(|| format!("information_schema check {table}.{column}"))?
    .try_get::<i64, _>(0)?;
    if exists > 0 {
        sqlx::query(&format!("ALTER TABLE {table} DROP COLUMN {column}"))
            .execute(pool)
            .await
            .with_context(|| format!("alter {table} drop {column} (mysql)"))?;
    }
    Ok(())
}

/// `chmod 0600` the admin db file (file-backed SQLite only). Best-effort.
#[cfg(unix)]
fn tighten_db_perms(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
        tracing::warn!(error = %e, path = %path.display(), "could not chmod admin db");
    }
}

#[cfg(not(unix))]
fn tighten_db_perms(_path: &Path) {}

/// BLAKE3 of the session token, hex-encoded — the `sessions` lookup key.
/// The cookie carries the raw token; we hash on insert/lookup/delete so a
/// leaked db only ever exposes the preimage-resistant hash.
pub fn hash_session_token(raw: &str) -> String {
    blake3::hash(raw.as_bytes()).to_hex().to_string()
}

/// Seconds since the unix epoch as `i64`.
pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ── Per-backend schema (fresh-start; all current columns present) ──────

const SQLITE_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS admin_users (
    username      TEXT PRIMARY KEY NOT NULL,
    password_hash TEXT NOT NULL,
    created_at    INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS sessions (
    token_hash TEXT PRIMARY KEY NOT NULL,
    username   TEXT NOT NULL,
    expires_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS sessions_expires_idx ON sessions(expires_at);
CREATE TABLE IF NOT EXISTS server_config (
    id              INTEGER PRIMARY KEY CHECK (id = 1),
    server_name     TEXT    NOT NULL DEFAULT '',
    max_peers       INTEGER NOT NULL DEFAULT 256,
    idle_kick_secs  INTEGER NOT NULL DEFAULT 10,
    grpc_password   TEXT    NOT NULL DEFAULT '',
    named_channels_enabled INTEGER NOT NULL DEFAULT 0,
    audio_quality   INTEGER NOT NULL DEFAULT 2,
    updated_at      INTEGER NOT NULL DEFAULT 0
);
INSERT OR IGNORE INTO server_config (id) VALUES (1);
CREATE TABLE IF NOT EXISTS channel_names (
    frequency  TEXT PRIMARY KEY NOT NULL,
    name       TEXT NOT NULL,
    updated_at INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS identities (
    pubkey           TEXT PRIMARY KEY NOT NULL,
    display_id       TEXT NOT NULL,
    last_callsign    TEXT NOT NULL DEFAULT '',
    machine_hash     TEXT NOT NULL DEFAULT '',
    origin_client_id TEXT NOT NULL DEFAULT '',
    first_seen       INTEGER NOT NULL,
    last_seen        INTEGER NOT NULL,
    last_ip          TEXT NOT NULL DEFAULT ''
);
CREATE TABLE IF NOT EXISTS metrics_samples (
    ts           INTEGER PRIMARY KEY NOT NULL,
    rx_bps       INTEGER NOT NULL,
    tx_bps       INTEGER NOT NULL,
    users        INTEGER NOT NULL,
    transmitting INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS metrics_ts_idx ON metrics_samples(ts);
CREATE TABLE IF NOT EXISTS audit_log (
    id        INTEGER PRIMARY KEY AUTOINCREMENT,
    ts        INTEGER NOT NULL,
    kind      TEXT NOT NULL,
    actor     TEXT NOT NULL,
    frequency TEXT NOT NULL DEFAULT '',
    detail    TEXT NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS audit_log_id_idx ON audit_log(id)
"#;

const MYSQL_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS admin_users (
    username      VARCHAR(255) PRIMARY KEY NOT NULL,
    password_hash TEXT NOT NULL,
    created_at    BIGINT NOT NULL
);
CREATE TABLE IF NOT EXISTS sessions (
    token_hash VARCHAR(64) PRIMARY KEY NOT NULL,
    username   VARCHAR(255) NOT NULL,
    expires_at BIGINT NOT NULL,
    INDEX sessions_expires_idx (expires_at)
);
CREATE TABLE IF NOT EXISTS server_config (
    id              INT PRIMARY KEY CHECK (id = 1),
    server_name     VARCHAR(255) NOT NULL DEFAULT '',
    max_peers       BIGINT NOT NULL DEFAULT 256,
    idle_kick_secs  BIGINT NOT NULL DEFAULT 10,
    grpc_password   VARCHAR(255) NOT NULL DEFAULT '',
    named_channels_enabled BIGINT NOT NULL DEFAULT 0,
    audio_quality   BIGINT NOT NULL DEFAULT 2,
    updated_at      BIGINT NOT NULL DEFAULT 0
);
INSERT IGNORE INTO server_config (id) VALUES (1);
CREATE TABLE IF NOT EXISTS channel_names (
    frequency  VARCHAR(32) PRIMARY KEY NOT NULL,
    name       VARCHAR(255) NOT NULL,
    updated_at BIGINT NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS identities (
    pubkey           VARCHAR(64) PRIMARY KEY NOT NULL,
    display_id       VARCHAR(32) NOT NULL,
    last_callsign    VARCHAR(16) NOT NULL DEFAULT '',
    machine_hash     VARCHAR(64) NOT NULL DEFAULT '',
    origin_client_id VARCHAR(64) NOT NULL DEFAULT '',
    first_seen       BIGINT NOT NULL,
    last_seen        BIGINT NOT NULL,
    last_ip          VARCHAR(64) NOT NULL DEFAULT ''
);
CREATE TABLE IF NOT EXISTS metrics_samples (
    ts           BIGINT PRIMARY KEY NOT NULL,
    rx_bps       BIGINT NOT NULL,
    tx_bps       BIGINT NOT NULL,
    users        BIGINT NOT NULL,
    transmitting BIGINT NOT NULL
);
CREATE TABLE IF NOT EXISTS audit_log (
    id        BIGINT NOT NULL AUTO_INCREMENT PRIMARY KEY,
    ts        BIGINT NOT NULL,
    kind      VARCHAR(64) NOT NULL,
    actor     VARCHAR(255) NOT NULL,
    frequency VARCHAR(32) NOT NULL DEFAULT '',
    detail    VARCHAR(1024) NOT NULL DEFAULT ''
)
"#;

const POSTGRES_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS admin_users (
    username      TEXT PRIMARY KEY NOT NULL,
    password_hash TEXT NOT NULL,
    created_at    BIGINT NOT NULL
);
CREATE TABLE IF NOT EXISTS sessions (
    token_hash TEXT PRIMARY KEY NOT NULL,
    username   TEXT NOT NULL,
    expires_at BIGINT NOT NULL
);
CREATE INDEX IF NOT EXISTS sessions_expires_idx ON sessions(expires_at);
CREATE TABLE IF NOT EXISTS server_config (
    id              INTEGER PRIMARY KEY CHECK (id = 1),
    server_name     TEXT    NOT NULL DEFAULT '',
    max_peers       BIGINT  NOT NULL DEFAULT 256,
    idle_kick_secs  BIGINT  NOT NULL DEFAULT 10,
    grpc_password   TEXT    NOT NULL DEFAULT '',
    named_channels_enabled BIGINT NOT NULL DEFAULT 0,
    audio_quality   BIGINT  NOT NULL DEFAULT 2,
    updated_at      BIGINT  NOT NULL DEFAULT 0
);
INSERT INTO server_config (id) VALUES (1) ON CONFLICT (id) DO NOTHING;
CREATE TABLE IF NOT EXISTS channel_names (
    frequency  TEXT PRIMARY KEY NOT NULL,
    name       TEXT NOT NULL,
    updated_at BIGINT NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS identities (
    pubkey           TEXT PRIMARY KEY NOT NULL,
    display_id       TEXT NOT NULL,
    last_callsign    TEXT NOT NULL DEFAULT '',
    machine_hash     TEXT NOT NULL DEFAULT '',
    origin_client_id TEXT NOT NULL DEFAULT '',
    first_seen       BIGINT NOT NULL,
    last_seen        BIGINT NOT NULL,
    last_ip          TEXT NOT NULL DEFAULT ''
);
CREATE TABLE IF NOT EXISTS metrics_samples (
    ts           BIGINT PRIMARY KEY NOT NULL,
    rx_bps       BIGINT NOT NULL,
    tx_bps       BIGINT NOT NULL,
    users        BIGINT NOT NULL,
    transmitting BIGINT NOT NULL
);
CREATE INDEX IF NOT EXISTS metrics_ts_idx ON metrics_samples(ts);
CREATE TABLE IF NOT EXISTS audit_log (
    id        BIGSERIAL PRIMARY KEY,
    ts        BIGINT NOT NULL,
    kind      TEXT NOT NULL,
    actor     TEXT NOT NULL,
    frequency TEXT NOT NULL DEFAULT '',
    detail    TEXT NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS audit_log_id_idx ON audit_log(id)
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn migration_is_idempotent() {
        let db = AdminDb::open_in_memory().await.unwrap();
        db.migrate().await.unwrap();
        db.migrate().await.unwrap();
        assert_eq!(db.user_count().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn user_insert_and_lookup_round_trip() {
        let db = AdminDb::open_in_memory().await.unwrap();
        db.migrate().await.unwrap();
        db.insert_user("admin", "$argon2id$fake$hash")
            .await
            .unwrap();
        assert_eq!(db.user_count().await.unwrap(), 1);
        let hash = db.get_password_hash("admin").await.unwrap().unwrap();
        assert_eq!(hash, "$argon2id$fake$hash");
        assert!(db.get_password_hash("ghost").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn session_round_trip_and_expiry() {
        let db = AdminDb::open_in_memory().await.unwrap();
        db.migrate().await.unwrap();
        db.insert_user("admin", "h").await.unwrap();

        let future = now_unix() + 3600;
        db.create_session("tok-valid", "admin", future)
            .await
            .unwrap();
        let row = db.lookup_session("tok-valid").await.unwrap().unwrap();
        assert_eq!(row.username, "admin");
        assert_eq!(row.expires_at, future);

        db.create_session("tok-expired", "admin", now_unix() - 10)
            .await
            .unwrap();
        assert!(db.lookup_session("tok-expired").await.unwrap().is_none());
        assert!(db.lookup_session("tok-ghost").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn raw_token_never_lands_on_disk() {
        let db = AdminDb::open_in_memory().await.unwrap();
        db.migrate().await.unwrap();
        db.insert_user("admin", "h").await.unwrap();
        let raw = "deadbeef-this-is-the-cookie-value";
        db.create_session(raw, "admin", now_unix() + 60)
            .await
            .unwrap();
        let stored = db
            .fetch_text("SELECT token_hash FROM sessions LIMIT 1")
            .await
            .unwrap();
        assert!(
            !stored.contains(raw),
            "raw token leaked into token_hash column"
        );
        assert_eq!(
            stored.len(),
            64,
            "expected 64-char BLAKE3 hex; got {}",
            stored.len()
        );
        let row = db.lookup_session(raw).await.unwrap().unwrap();
        assert_eq!(row.username, "admin");
    }

    #[tokio::test]
    async fn delete_session_is_idempotent() {
        let db = AdminDb::open_in_memory().await.unwrap();
        db.migrate().await.unwrap();
        db.insert_user("admin", "h").await.unwrap();
        db.create_session("tok", "admin", now_unix() + 60)
            .await
            .unwrap();
        db.delete_session("tok").await.unwrap();
        db.delete_session("tok").await.unwrap();
        db.delete_session("never-existed").await.unwrap();
    }

    #[tokio::test]
    async fn update_password_hash_overwrites_existing_row() {
        let db = AdminDb::open_in_memory().await.unwrap();
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
        let db = AdminDb::open_in_memory().await.unwrap();
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
        let db = AdminDb::open_in_memory().await.unwrap();
        db.migrate().await.unwrap();
        let cfg = db.load_server_config().await.unwrap();
        assert_eq!(cfg.server_name, "");
        assert_eq!(cfg.max_peers, 256);
        assert_eq!(cfg.idle_kick_secs, 10);
        assert_eq!(cfg.audio_quality, 2);
    }

    #[tokio::test]
    async fn server_config_round_trips() {
        let db = AdminDb::open_in_memory().await.unwrap();
        db.migrate().await.unwrap();
        let new = ServerConfig {
            server_name: "Singular Toki".into(),
            max_peers: 1024,
            idle_kick_secs: 30,
            grpc_password: "hunter2".into(),
            named_channels_enabled: true,
            audio_quality: 1,
        };
        db.save_server_config(&new).await.unwrap();
        let loaded = db.load_server_config().await.unwrap();
        assert_eq!(loaded.server_name, "Singular Toki");
        assert_eq!(loaded.max_peers, 1024);
        assert_eq!(loaded.idle_kick_secs, 30);
        assert_eq!(loaded.grpc_password, "hunter2");
        assert!(loaded.named_channels_enabled);
        assert_eq!(loaded.audio_quality, 1);
    }

    #[tokio::test]
    async fn migrate_adds_columns_to_pre_existing_sqlite_table() {
        // A SQLite db created by a binary that predates later columns
        // should grow them on migrate(). Build the old shape, migrate,
        // verify load/save round-trips the new columns.
        let db = AdminDb::open_in_memory().await.unwrap();
        for stmt in [
            "CREATE TABLE server_config (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                server_name TEXT NOT NULL DEFAULT '',
                max_peers INTEGER NOT NULL DEFAULT 256,
                idle_kick_secs INTEGER NOT NULL DEFAULT 10,
                updated_at INTEGER NOT NULL DEFAULT 0
            )",
            "INSERT INTO server_config (id) VALUES (1)",
        ] {
            db.exec_raw(stmt).await.unwrap();
        }
        db.migrate().await.unwrap();
        db.save_server_config(&ServerConfig {
            server_name: "after-upgrade".into(),
            max_peers: 128,
            idle_kick_secs: 7,
            grpc_password: "secret".into(),
            named_channels_enabled: true,
            audio_quality: 1,
        })
        .await
        .unwrap();
        let loaded = db.load_server_config().await.unwrap();
        assert_eq!(loaded.grpc_password, "secret");
        assert!(loaded.named_channels_enabled);
        assert_eq!(loaded.audio_quality, 1);
        db.migrate().await.unwrap(); // still idempotent
    }

    #[tokio::test]
    async fn server_config_is_singleton() {
        let db = AdminDb::open_in_memory().await.unwrap();
        db.migrate().await.unwrap();
        let err = db
            .exec_raw("INSERT INTO server_config (id, server_name) VALUES (2, 'rogue')")
            .await;
        assert!(err.is_err(), "second row should be rejected by CHECK");
    }

    #[tokio::test]
    async fn channel_names_crud_roundtrips() {
        let db = AdminDb::open_in_memory().await.unwrap();
        db.migrate().await.unwrap();
        assert!(db.load_channel_names().await.unwrap().is_empty());

        db.set_channel_name("446.05", "Ops Net").await.unwrap();
        db.set_channel_name("447.00", "Backup").await.unwrap();
        db.set_channel_name("446.05", "Dispatch").await.unwrap(); // upsert
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

    fn identity_record(first_seen: i64) -> IdentityRecord {
        IdentityRecord {
            display_id: "FLNIHQMB".into(),
            last_callsign: "coton".into(),
            machine_hash: "ab".repeat(32),
            origin_client_id: String::new(),
            first_seen,
            last_seen: first_seen,
            last_ip: "10.0.0.1".into(),
        }
    }

    #[tokio::test]
    async fn migrate_drops_stale_first_callsign_column() {
        // A database created by a pre-release dev build has an
        // `identities` table with a NOT NULL `first_callsign` column
        // the current schema no longer supplies — inserts would
        // violate the constraint. migrate() must drop it.
        let db = AdminDb::open_in_memory().await.unwrap();
        db.exec_raw(
            "CREATE TABLE identities (
                pubkey           TEXT PRIMARY KEY NOT NULL,
                display_id       TEXT NOT NULL,
                first_callsign   TEXT NOT NULL,
                last_callsign    TEXT NOT NULL DEFAULT '',
                machine_hash     TEXT NOT NULL DEFAULT '',
                origin_client_id TEXT NOT NULL DEFAULT '',
                first_seen       INTEGER NOT NULL,
                last_seen        INTEGER NOT NULL,
                last_ip          TEXT NOT NULL DEFAULT ''
            )",
        )
        .await
        .unwrap();
        db.migrate().await.unwrap();
        db.upsert_identity("aa11", &identity_record(100))
            .await
            .expect("upsert must work after the stale column is dropped");
        db.migrate().await.unwrap(); // still idempotent
        assert_eq!(db.load_identities().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn identities_upsert_and_load_round_trip() {
        let db = AdminDb::open_in_memory().await.unwrap();
        db.migrate().await.unwrap();
        assert!(db.load_identities().await.unwrap().is_empty());

        db.upsert_identity("aa11", &identity_record(100))
            .await
            .unwrap();
        let map = db.load_identities().await.unwrap();
        assert_eq!(map.len(), 1);
        let rec = &map["aa11"];
        assert_eq!(rec.display_id, "FLNIHQMB");
        assert_eq!(rec.first_seen, 100);
        assert_eq!(rec.machine_hash, "ab".repeat(32));
    }

    #[tokio::test]
    async fn identities_conflict_keeps_immutable_facts() {
        let db = AdminDb::open_in_memory().await.unwrap();
        db.migrate().await.unwrap();
        db.upsert_identity("aa11", &identity_record(100))
            .await
            .unwrap();

        // A later register (even one fed through a boot race claiming a
        // different history) must not rewrite first_seen, and origin is
        // first-non-empty-wins.
        let mut later = identity_record(999); // wrong first_seen on purpose
        later.last_callsign = "renamed".into();
        later.last_seen = 200;
        later.last_ip = "10.0.0.2".into();
        later.origin_client_id = "origin-1".into();
        db.upsert_identity("aa11", &later).await.unwrap();

        let rec = &db.load_identities().await.unwrap()["aa11"];
        assert_eq!(rec.first_seen, 100, "first_seen immutable");
        assert_eq!(rec.last_callsign, "renamed");
        assert_eq!(rec.last_seen, 200);
        assert_eq!(rec.last_ip, "10.0.0.2");
        assert_eq!(rec.origin_client_id, "origin-1", "filled once");

        // A third upsert with a different origin doesn't overwrite it.
        let mut third = identity_record(100);
        third.origin_client_id = "origin-2".into();
        db.upsert_identity("aa11", &third).await.unwrap();
        let rec = &db.load_identities().await.unwrap()["aa11"];
        assert_eq!(rec.origin_client_id, "origin-1", "first non-empty wins");
    }

    #[tokio::test]
    async fn metrics_insert_load_prune() {
        let db = AdminDb::open_in_memory().await.unwrap();
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
        assert_eq!(db.load_metrics(250).await.unwrap().len(), 1);
        db.prune_metrics(200).await.unwrap();
        let kept = db.load_metrics(0).await.unwrap();
        assert_eq!(kept.len(), 2);
        assert_eq!(kept[0].ts, 200);
    }

    #[tokio::test]
    async fn metrics_upsert_on_duplicate_ts() {
        let db = AdminDb::open_in_memory().await.unwrap();
        db.migrate().await.unwrap();
        db.insert_metric_sample(100, 1, 1, 1, 0).await.unwrap();
        db.insert_metric_sample(100, 999, 2, 2, 1).await.unwrap(); // same ts → replace
        let all = db.load_metrics(0).await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].rx_bps, 999);
    }

    #[tokio::test]
    async fn audit_insert_load_filter_page_prune() {
        let db = AdminDb::open_in_memory().await.unwrap();
        db.migrate().await.unwrap();
        db.insert_audit(10, "connect", "A", "", "").await.unwrap();
        db.insert_audit(20, "kick", "admin", "446.05", "")
            .await
            .unwrap();
        db.insert_audit(30, "auth-fail", "SYSTEM", "", "")
            .await
            .unwrap();

        let (rows, total) = db.load_audit(&[], 50, 0).await.unwrap();
        assert_eq!(total, 3);
        assert_eq!(rows[0].kind, "auth-fail");

        let (rows, total) = db.load_audit(&["kick", "rename"], 50, 0).await.unwrap();
        assert_eq!(total, 1);
        assert_eq!(rows[0].kind, "kick");

        let newest_id = db.load_audit(&[], 1, 0).await.unwrap().0[0].id;
        let (page2, _) = db.load_audit(&[], 50, newest_id).await.unwrap();
        assert_eq!(page2.len(), 2);
        assert!(page2.iter().all(|r| r.id < newest_id));

        db.prune_audit(25).await.unwrap();
        let (rows, total) = db.load_audit(&[], 50, 0).await.unwrap();
        assert_eq!(total, 1);
        assert_eq!(rows[0].kind, "auth-fail");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn open_chmods_db_to_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("toki-db-perms-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("admin.db");
        let url = format!("sqlite://{}?mode=rwc", path.display());
        let _db = AdminDb::open(&url).await.unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(mode, 0o600, "admin.db must be chmod 0600, was {mode:o}");
    }

    #[tokio::test]
    async fn prune_drops_only_expired_rows() {
        let db = AdminDb::open_in_memory().await.unwrap();
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

    #[tokio::test]
    async fn connect_with_retry_recovers_from_transient_failure() {
        // First attempt "fails" (DB not up yet), second succeeds. Proves
        // the backoff loop retries and then returns the value.
        let calls = std::cell::Cell::new(0u32);
        let got: Result<u32> = connect_with_retry("test", || {
            calls.set(calls.get() + 1);
            let n = calls.get();
            async move {
                if n < 2 {
                    Err(sqlx::Error::PoolClosed)
                } else {
                    Ok(42)
                }
            }
        })
        .await;
        assert_eq!(got.unwrap(), 42);
        assert_eq!(calls.get(), 2, "should have retried exactly once");
    }

    #[test]
    fn pg_rewrite_numbers_placeholders() {
        assert_eq!(pg_rewrite("a = ? AND b = ?"), "a = $1 AND b = $2");
        assert_eq!(pg_rewrite("no params"), "no params");
    }

    #[test]
    fn detect_backend_by_scheme() {
        assert_eq!(detect_backend("sqlite::memory:").unwrap(), Backend::Sqlite);
        assert_eq!(
            detect_backend("sqlite://x?mode=rwc").unwrap(),
            Backend::Sqlite
        );
        assert_eq!(detect_backend("mysql://u@h/d").unwrap(), Backend::MySql);
        assert_eq!(detect_backend("mariadb://u@h/d").unwrap(), Backend::MySql);
        assert_eq!(
            detect_backend("postgres://u@h/d").unwrap(),
            Backend::Postgres
        );
        assert_eq!(
            detect_backend("postgresql://u@h/d").unwrap(),
            Backend::Postgres
        );
        assert!(detect_backend("redis://x").is_err());
    }
}
