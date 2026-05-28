//! SQLite-backed admin user + session store.
//!
//! Two tables, both created idempotently on first boot via [`AdminDb::migrate`]:
//!
//! * `admin_users(username PK, password_hash, created_at)` — one row per
//!   admin account. We never read passwords in cleartext; only the
//!   argon2id hash is stored. v1 ships with a single seeded `admin` user
//!   and no UI to create more (deliberate — multi-admin is a follow-up).
//!
//! * `sessions(token PK, username, expires_at)` — opaque cookie tokens
//!   issued by `/api/login`. Token is 32 hex bytes (16 bytes of CSPRNG
//!   entropy from `rand::rngs::OsRng`). Stored as-is (not hashed) for v1;
//!   the threat model assumes the sqlite file itself is operator-
//!   controlled, and an attacker with the file already has the password
//!   hash to brute force. Hashing session tokens is a follow-up.
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
                    token      TEXT PRIMARY KEY NOT NULL,
                    username   TEXT NOT NULL,
                    expires_at INTEGER NOT NULL,
                    FOREIGN KEY (username) REFERENCES admin_users(username)
                );
                CREATE INDEX IF NOT EXISTS sessions_expires_idx
                    ON sessions(expires_at);
                "#,
            )?;
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
    pub async fn create_session(&self, token: &str, username: &str, expires_at: i64) -> Result<()> {
        let token = token.to_string();
        let username = username.to_string();
        self.with_conn(move |c| {
            c.execute(
                "INSERT INTO sessions (token, username, expires_at) VALUES (?1, ?2, ?3)",
                params![token, username, expires_at],
            )?;
            Ok(())
        })
        .await
    }

    /// Resolve a cookie token back to its (username, expiry). Returns
    /// `None` if the token is unknown *or* expired — callers don't need
    /// to differentiate; both map to 401.
    pub async fn lookup_session(&self, token: &str) -> Result<Option<SessionRow>> {
        let token = token.to_string();
        let now = now_unix();
        self.with_conn(move |c| {
            c.query_row(
                "SELECT username, expires_at FROM sessions WHERE token = ?1 AND expires_at > ?2",
                params![token, now],
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
    /// token returns `Ok(())`. Called by `/api/logout`.
    pub async fn delete_session(&self, token: &str) -> Result<()> {
        let token = token.to_string();
        self.with_conn(move |c| {
            c.execute("DELETE FROM sessions WHERE token = ?1", params![token])?;
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

    /// Internal helper: run a closure with locked, blocking access to
    /// the sqlite connection on a worker thread. Every public method
    /// goes through this so the async signatures stay uniform.
    async fn with_conn<F, R>(&self, f: F) -> Result<R>
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
