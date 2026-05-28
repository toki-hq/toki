//! Admin web panel.
//!
//! A small axum HTTP service exposed on a separate port (default
//! `8000`), bound to the same process as the gRPC signaling server.
//! Surfaces the live registry state (clients per frequency, current
//! PTT holders) over Server-Sent Events and exposes three operator
//! actions — **kick**, **move to frequency**, **rename callsign** —
//! that mutate the same `Arc<Mutex<Registry>>` the signaling handlers
//! use, so admin actions and client-driven lifecycle events stay
//! consistent.
//!
//! # Why HTTP, not gRPC
//!
//! Browsers can't speak HTTP/2-framed gRPC directly; gRPC-Web works
//! but requires generated JavaScript stubs (a build step). We picked
//! "vanilla HTML/JS, embedded, no build pipeline", which is at odds
//! with that. SSE for one-way server→client pushes + REST/JSON for
//! mutations is the natural fit for the constraint and is trivial to
//! implement in axum.
//!
//! # Auth bootstrap
//!
//! On first boot, [`run`] checks the sqlite store at `db_path`. If
//! the `admin_users` table is empty, it seeds a single user named
//! `admin` with a freshly-generated 24-char password and logs the
//! credentials once at `WARN`. The operator copies them out of the
//! journal; only the argon2id hash remains on disk thereafter. There
//! is intentionally no UI to recover a lost password — `rm admin.db`
//! and restart the server to re-seed.
//!
//! # Threading
//!
//! The admin task runs alongside `signaling`, `audio`, and `reaper`
//! in the top-level `tokio::select!`. It shares only the
//! [`SharedRegistry`] handle; everything else (sqlite, sessions,
//! broadcaster) is owned by this module.

use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::config::AdminConfig;
use crate::state::SharedRegistry;

pub mod auth;
pub mod db;
pub mod dto;
pub mod handlers;
pub mod routes;
pub mod sse;

/// Concrete shared state for axum handlers. `Clone` is shallow —
/// every field is either `Arc`-internal or itself `Clone` — so axum's
/// per-request state extraction never deep-copies.
#[derive(Clone)]
pub struct AppState {
    /// The signaling registry. Locked briefly for snapshots and for
    /// the mutations behind kick / move / rename.
    pub registry: SharedRegistry,
    /// SQLite-backed admin user + session store.
    pub db: db::AdminDb,
    /// Broadcast channel for `/api/events`. Periodic snapshots are
    /// fanned out to all connected SSE clients. Lagging consumers
    /// (slow browsers) drop intermediate snapshots rather than
    /// blocking the publisher — see [`sse::run_broadcaster`].
    pub broadcaster: tokio::sync::broadcast::Sender<dto::Snapshot>,
    /// How long an issued session cookie is valid. Set once at
    /// startup from `AdminConfig.session_ttl_hours` and carried in
    /// the state so handlers don't have to re-read config.
    pub session_ttl: Duration,
}

/// Entry point for the admin task. Opens (and migrates) the sqlite
/// store, seeds the `admin` user if needed, spawns the periodic
/// snapshot broadcaster, and serves the axum router until the
/// listener errors out.
///
/// Returns `Err` only on unrecoverable startup or I/O failures —
/// the caller (`main.rs`) selects on this future alongside the
/// other server tasks, so any error here brings the whole process
/// down rather than leaving a half-running server.
pub async fn run(cfg: AdminConfig, registry: SharedRegistry) -> Result<()> {
    let bind: SocketAddr = format!("{}:{}", cfg.bind, cfg.port)
        .parse()
        .with_context(|| format!("parse admin bind addr {}:{}", cfg.bind, cfg.port))?;

    // Open + migrate sqlite before we touch the network. A migration
    // failure should fail fast at startup, not on the first request.
    let db = db::AdminDb::open(&cfg.db_path)
        .with_context(|| format!("open admin sqlite at {}", cfg.db_path.display()))?;
    db.migrate().await.context("migrate admin sqlite")?;

    // Seed `admin` user if the store is empty. We log the generated
    // password once at WARN level — this is the operator's only
    // chance to capture it.
    auth::seed_admin_if_empty(&db).await?;

    // Broadcast channel: capacity 16 is plenty for ~1Hz snapshots;
    // slow consumers fall behind a few seconds at worst before
    // tokio_stream's BroadcastStream emits a `Lagged` they recover
    // from on the next tick.
    let (tx, _) = tokio::sync::broadcast::channel::<dto::Snapshot>(16);

    let state = AppState {
        registry: registry.clone(),
        db,
        broadcaster: tx.clone(),
        session_ttl: Duration::from_secs(cfg.session_ttl_hours * 3600),
    };

    // Periodic snapshot loop. Lives for the lifetime of the admin
    // task; aborted implicitly when this function returns (its tokio
    // task is detached but tied to the runtime, which exits with main).
    tokio::spawn(sse::run_broadcaster(registry, tx));

    let router = routes::build(state);
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .with_context(|| format!("bind admin listener on {bind}"))?;
    tracing::info!(%bind, "admin panel listening (HTTP)");

    axum::serve(listener, router)
        .await
        .context("admin axum::serve")?;
    Ok(())
}
