//! Admin web panel.
//!
//! A gRPC-Web control-plane service served on the admin TLS listener
//! (default `8000`) in the same process as the gRPC signaling server. The
//! admin SPA ships separately (the standalone `admin-ui/` service) and
//! reaches this listener through a reverse proxy; it talks gRPC-Web to the
//! [`grpc::AdminApi`] service, which surfaces live registry state over the
//! streaming `Watch` RPC and exposes operator actions (kick / move /
//! rename / priority) + runtime config, all mutating the same
//! `Arc<Mutex<Registry>>` the signaling handlers use.
//!
//! # Serving
//!
//! One [`axum::Router`] on the TLS listener: the `GrpcWebLayer`-wrapped
//! `AdminServer` (mounted at `/toki.admin.v1.Admin/*` via tonic's
//! `Routes::into_axum_router`) and the two cookie endpoints
//! (`/api/login`, `/api/logout`). The server no longer serves any static
//! assets — that's the standalone UI service's job.
//!
//! # Auth
//!
//! Login/logout are plain HTTP because they set/clear the HttpOnly
//! session cookie. Every gRPC-Web call carries that cookie (same origin);
//! [`grpc::AuthInterceptor`] + a per-RPC async guard validate it against
//! the session DB.
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
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use axum::{
    extract::{OriginalUri, State},
    http::{header, HeaderMap, StatusCode},
    response::Redirect,
    routing::any,
    Router,
};
use axum_server::tls_rustls::RustlsConfig;
use tonic_web::GrpcWebLayer;

use toki_proto::admin::v1::admin_server::AdminServer;

use crate::audit::AuditSink;
use crate::config::AdminConfig;
use crate::metrics::{SharedByteCounters, SharedHealth, SharedLiveRate};
use crate::server_config::SharedServerConfig;
use crate::state::{DuplexMode, SharedChannelNames, SharedDuplexModes, SharedRegistry};
use crate::throttle::IpThrottle;
use crate::tls::TlsMaterial;

pub mod auth;
pub mod db;
pub mod grpc;
pub mod handlers;
pub mod routes;
pub mod watch;

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
    /// Broadcast channel feeding the gRPC `Watch` stream. Periodic
    /// snapshots (1 Hz) plus an immediate push after every mutation are
    /// fanned out to all connected admin browsers. Lagging consumers
    /// drop intermediate snapshots rather than blocking the publisher —
    /// see [`watch::run_broadcaster`] / [`watch::broadcast_stream`].
    pub broadcaster: tokio::sync::broadcast::Sender<toki_proto::admin::v1::Snapshot>,
    /// How long an issued session cookie is valid. Set once at
    /// startup from `AdminConfig.session_ttl_hours` and carried in
    /// the state so handlers don't have to re-read config.
    pub session_ttl: Duration,
    /// Process startup instant. Used by the broadcaster to fill
    /// `Snapshot.server_uptime_secs` and by `/api/server-info` to
    /// emit `started_at_unix`. Captured at `admin::run` start, which
    /// is close enough to "main started" that the offset is in the
    /// noise floor — admin uptime is what operators actually care
    /// about anyway.
    pub started_at: Instant,
    /// Bound admin listen address as a string ("ip:port"), echoed
    /// back from `/api/server-info` for the header's HOST stat.
    pub admin_bind: String,
    /// Per-source-IP rate cap + exponential auth-failure backoff for
    /// `/api/login`. Same `IpThrottle` machinery the gRPC `Register`
    /// RPC uses, instantiated separately because the admin surface
    /// has different traffic characteristics — they shouldn't share
    /// a backoff state. Wrapped in `Arc` so `AppState.clone()` stays
    /// shallow.
    pub login_throttle: Arc<IpThrottle>,
    /// Live, runtime-mutable server settings (server_name, max_peers,
    /// idle_kick_secs, grpc_password). Same handle the gRPC signaling
    /// service and reaper read from; the admin's PUT handlers update
    /// this in lockstep with the sqlite row so the new value takes
    /// effect immediately on every subsystem.
    pub server_config: SharedServerConfig,
    /// Admin-assigned channel names (frequency → name). Shared with the
    /// signaling service (the reader). The name RPCs write both this
    /// map and the `channel_names` sqlite table in lockstep, and the
    /// broadcaster folds it into each `Snapshot` so the panel can label
    /// every frequency — occupied or not.
    pub channel_names: SharedChannelNames,
    /// Per-frequency duplex modes (frequency → [`DuplexMode`]). Shared
    /// with the signaling service (the reader). `SetChannelMode` writes
    /// this map, the `channel_modes` sqlite table, and any live room's
    /// `Room.duplex` in lockstep; the broadcaster folds it into each
    /// `Snapshot`.
    pub duplex_modes: SharedDuplexModes,
    /// Channel-wide mutes (frequency set), shared with the signaling
    /// speak-gate. The `SetChannelMute` RPC writes the db and this set
    /// in lockstep so a channel mute takes effect on the next PTT press;
    /// the broadcaster folds it into each `Snapshot` so the panel can
    /// flag muted channels (occupied or not).
    pub channel_mutes: crate::state::SharedChannelMutes,
    /// Active identity bans (banned pubkey → record), shared with the
    /// signaling register gate. The ban / lift RPCs write the db and
    /// this map in lockstep so a ban takes effect on the very next
    /// register attempt.
    pub bans: crate::state::SharedBans,
    /// Latest host-health snapshot (CPU / memory / disk), refreshed by
    /// the metrics sampler. `GetServerHealth` clones it out.
    pub health: SharedHealth,
    /// Latest 1 Hz voice throughput, written by the snapshot broadcaster
    /// and folded into each `Watch` snapshot for the live bandwidth trace.
    pub live_rate: SharedLiveRate,
    /// Audit-log sink. Admin RPCs push their actions here; the writer
    /// task drains it to sqlite. Login (auth-ok / auth-fail) uses it too.
    pub audit: AuditSink,
    /// `true` when `config.toml` set a `password`. The TOML value
    /// takes precedence at the signaling layer (see `SignalingSvc`)
    /// and we surface this flag to the UI so the server-password
    /// input can be greyed out — a PUT on `/api/server-password`
    /// while this is true returns 409.
    pub toml_password_override: bool,
}

/// Entry point for the admin task. Opens (and migrates) the sqlite
/// store, seeds the `admin` user if needed, spawns the periodic
/// snapshot broadcaster, and serves the axum router over HTTPS
/// until the listener errors out.
///
/// `tls_material` is shared with the gRPC channel — either the
/// operator-provided cert+key or the rcgen-generated self-signed
/// pair from `tls/{cert,key}.pem`. We don't run a separate cert
/// for admin in v1; the panel and the gRPC port both serve the
/// same identity so operators can pin one fingerprint.
///
/// Returns `Err` only on unrecoverable startup or I/O failures —
/// the caller (`main.rs`) selects on this future alongside the
/// other server tasks, so any error here brings the whole process
/// down rather than leaving a half-running server.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    cfg: AdminConfig,
    registry: SharedRegistry,
    tls_material: TlsMaterial,
    server_config: SharedServerConfig,
    channel_names: SharedChannelNames,
    duplex_modes: SharedDuplexModes,
    channel_mutes: crate::state::SharedChannelMutes,
    identities: crate::state::SharedIdentities,
    mut identity_rx: tokio::sync::mpsc::UnboundedReceiver<(String, crate::state::IdentityRecord)>,
    bans: crate::state::SharedBans,
    byte_counters: SharedByteCounters,
    audit: AuditSink,
    audit_rx: tokio::sync::mpsc::UnboundedReceiver<crate::audit::AuditEvent>,
    data_dir: std::path::PathBuf,
    toml_password_override: bool,
) -> Result<()> {
    let bind: SocketAddr = format!("{}:{}", cfg.bind, cfg.port)
        .parse()
        .with_context(|| format!("parse admin bind addr {}:{}", cfg.bind, cfg.port))?;

    // Open + migrate the admin store before we touch the network. A
    // connection or migration failure should fail fast at startup, not
    // on the first request. The backend (SQLite / MariaDB / Postgres) is
    // chosen by the resolved connection URL.
    let database_url = cfg.resolve_database_url();
    let db = db::AdminDb::open(&database_url)
        .await
        .with_context(|| format!("open admin db ({})", redact_db_url(&database_url)))?;
    tracing::info!(
        backend = db.backend_label(),
        target = %redact_db_url(&database_url),
        "admin store opened",
    );
    db.migrate().await.context("migrate admin db")?;

    // Hydrate the in-memory server_config from sqlite. main.rs
    // constructed it with defaults; the row may have non-default
    // values if the operator's previously edited them via the UI.
    // Done before seeding the admin user so the rest of the panel
    // sees the current settings from request #1.
    {
        let loaded = db
            .load_server_config()
            .await
            .context("load server_config from admin db")?;
        *server_config.write().await = loaded;
    }

    // Hydrate the in-memory channel-name map from sqlite for the same
    // reason: main.rs handed us an empty map; the table may hold names
    // an operator set in a previous run. Signaling reads this map on
    // join, so it must reflect the persisted state from request #1.
    {
        let loaded = db
            .load_channel_names()
            .await
            .context("load channel_names from admin db")?;
        *channel_names.write().await = loaded;
    }

    // Same for the per-frequency duplex modes: hydrate from the
    // `channel_modes` table so signaling sees the operator's choices
    // from request #1 (the stored integer maps 0=half / 1=full).
    {
        let loaded = db
            .load_channel_modes()
            .await
            .context("load channel_modes from admin db")?;
        *duplex_modes.write().await = loaded
            .into_iter()
            .map(|(freq, m)| (freq, DuplexMode::from_u32(m)))
            .collect();
    }

    // Same for channel mutes — the signaling speak-gate reads this set
    // on every PTT press, so it must hold the persisted muted channels
    // before the first press lands.
    {
        let loaded = db
            .load_channel_mutes()
            .await
            .context("load channel_mutes from admin db")?;
        *channel_mutes.write().await = loaded;
    }

    // Hydrate the shared identity map. Same dance again: signaling
    // merges every identity-ful register against this map (first_seen
    // and a recorded origin survive restarts), so it must hold the
    // persisted records before the first register lands.
    {
        let loaded = db
            .load_identities()
            .await
            .context("load identities from admin db")?;
        *identities.write().await = loaded;
    }

    // Hydrate the shared ban map — the register gate consults it, so
    // persisted bans must be enforceable from the first register.
    {
        let loaded = db.load_bans().await.context("load bans from admin db")?;
        *bans.write().await = loaded;
    }

    // Seed `admin` user if the store is empty. We log the generated
    // password once at WARN level — this is the operator's only
    // chance to capture it.
    auth::seed_admin_if_empty(&db).await?;

    // Broadcast channel: capacity 16 is plenty for ~1Hz snapshots;
    // slow consumers fall behind a few seconds at worst before
    // tokio_stream's BroadcastStream emits a `Lagged` they recover
    // from on the next tick.
    let (tx, _) = tokio::sync::broadcast::channel::<toki_proto::admin::v1::Snapshot>(16);

    // Shared host-health snapshot, written by the metrics sampler and
    // read by `GetServerHealth`.
    let health = crate::metrics::shared_health();
    // Live throughput, written by the broadcaster, read into snapshots.
    let live_rate = crate::metrics::shared_live_rate();

    let started_at = Instant::now();
    let admin_bind = bind.to_string();
    let state = AppState {
        registry: registry.clone(),
        db: db.clone(),
        broadcaster: tx.clone(),
        session_ttl: Duration::from_secs(cfg.session_ttl_hours * 3600),
        started_at,
        admin_bind,
        login_throttle: Arc::new(IpThrottle::new()),
        server_config,
        channel_names: channel_names.clone(),
        duplex_modes: duplex_modes.clone(),
        channel_mutes: channel_mutes.clone(),
        bans,
        health: health.clone(),
        live_rate: live_rate.clone(),
        audit,
        toml_password_override,
    };

    // Audit writer: drains the audit channel into sqlite for the life of
    // the process (the only task that writes the audit_log table).
    tokio::spawn(crate::audit::run_writer(audit_rx, db.clone()));

    // Identity writer: drains the identity channel into the identities
    // table — same split as audit (signaling produces, the db owner
    // persists). A failed upsert logs and moves on: the in-memory map
    // still has the record, so only durability across a restart is at
    // stake, never the live session.
    {
        let db = db.clone();
        tokio::spawn(async move {
            while let Some((pubkey, record)) = identity_rx.recv().await {
                if let Err(e) = db.upsert_identity(&pubkey, &record).await {
                    tracing::warn!(error = %e, identity = %record.display_id, "could not persist identity");
                }
            }
        });
    }

    // Metrics sampler: refreshes host health + persists the 1-minute
    // bandwidth/users time-series, pruning past the retention window.
    tokio::spawn(crate::metrics::run_sampler(
        byte_counters.clone(),
        registry.clone(),
        db,
        health,
        data_dir,
    ));

    // Periodic snapshot loop. Lives for the lifetime of the admin
    // task; aborted implicitly when this function returns (its tokio
    // task is detached but tied to the runtime, which exits with main).
    tokio::spawn(watch::run_broadcaster(
        registry,
        channel_names,
        duplex_modes,
        channel_mutes,
        byte_counters,
        live_rate,
        tx,
        started_at,
    ));

    // Build the gRPC-Web Admin service: the generated server wrapped by
    // the cookie auth interceptor, exposed as an axum Router (tonic 0.13
    // `Routes::into_axum_router`), then layered with `GrpcWebLayer` so the
    // browser can call it over a plain fetch. Its routes live under
    // `/toki.admin.v1.Admin/*`; merging with the HTTP router is
    // unambiguous (no path overlap with `/api/*`).
    let admin_grpc =
        AdminServer::with_interceptor(grpc::AdminApi::new(state.clone()), grpc::AuthInterceptor);
    let grpc_router = tonic::service::Routes::new(admin_grpc)
        .into_axum_router()
        .layer(GrpcWebLayer::new());

    // Merge the cookie endpoints (no fallback) with the gRPC router, which
    // carries tonic's own fallback for unmatched paths. The SPA is served
    // by the standalone `admin-ui/` service, which reverse-proxies
    // `/api/*` + `/toki.admin.v1.Admin/*` back here.
    let router = routes::build(state).merge(grpc_router);

    // Build a RustlsConfig from the PEM bytes we already have in
    // hand. `from_pem` parses them via `rustls-pemfile`; the result
    // is reloadable but we don't expose that — operators who rotate
    // certs can restart the server. Same identity as the gRPC
    // channel: cert source is the operator's `[tls]` block, or our
    // auto-generated `tls/cert.pem` on first boot.
    let tls_cfg =
        RustlsConfig::from_pem(tls_material.cert_pem.clone(), tls_material.key_pem.clone())
            .await
            .context("admin: build RustlsConfig from PEM")?;

    tracing::info!(
        %bind,
        cert_source = %tls_material.source.display(),
        "admin panel listening (HTTPS)",
    );

    // Optional plain-HTTP listener that 308-redirects every request
    // to the HTTPS counterpart. The admin panel is TLS-only, so an
    // operator who types `http://host:8000` would otherwise get a
    // raw TLS-handshake error. When `http_redirect_port` is set, we
    // bind a second listener on `bind:that_port` that returns a 308
    // for every method + path; modern browsers preserve the method
    // (so POST/PUT survive) and cache the upgrade per-origin.
    let redirect_fut: std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send>> =
        match cfg.http_redirect_port {
            Some(http_port) => {
                let http_bind: SocketAddr = format!("{}:{}", cfg.bind, http_port)
                    .parse()
                    .with_context(|| {
                        format!("parse admin HTTP redirect bind {}:{}", cfg.bind, http_port)
                    })?;
                tracing::info!(
                    %http_bind,
                    https_port = cfg.port,
                    "admin HTTP→HTTPS redirect listening",
                );
                Box::pin(serve_redirect(http_bind, cfg.port))
            }
            None => Box::pin(std::future::pending()),
        };

    // `into_make_service_with_connect_info::<SocketAddr>` populates
    // the `ConnectInfo<SocketAddr>` extractor on every request — the
    // login handler reads the peer IP from there for its per-IP
    // rate-limit gate. Without it the extractor returns None and
    // the throttle wouldn't fire.
    let serve_fut = async move {
        axum_server::bind_rustls(bind, tls_cfg)
            .serve(router.into_make_service_with_connect_info::<SocketAddr>())
            .await
            .context("admin axum_server::bind_rustls")
    };

    // Both listeners run concurrently. If either errors we bring the
    // whole admin task down — `main.rs` will see this through the
    // `tokio::select!` and exit the process so the operator sees the
    // failure rather than a half-working admin surface.
    tokio::select! {
        res = serve_fut => res?,
        res = redirect_fut => res?,
    }
    Ok(())
}

/// Plain-HTTP listener that 308-redirects every request to the HTTPS
/// admin port. The handler reconstructs the canonical URL from the
/// inbound `Host` header (stripping any port the client supplied) and
/// the original path-and-query; the target host:port is
/// `<bare_host>:<https_port>`. 308 (vs 301/302) preserves the request
/// method, which matters for the JS shell's `fetch(...)` mutations.
/// Mask the password in a DB connection URL for logging:
/// `mysql://user:secret@host/db` → `mysql://user:***@host/db`. SQLite
/// URLs (no credentials) pass through unchanged.
fn redact_db_url(url: &str) -> String {
    if let Some(scheme_end) = url.find("://") {
        let (head, rest) = url.split_at(scheme_end + 3);
        if let Some(at) = rest.find('@') {
            let authority = &rest[..at];
            let tail = &rest[at..]; // includes '@'
            if let Some(colon) = authority.find(':') {
                return format!("{head}{}:***{tail}", &authority[..colon]);
            }
        }
    }
    url.to_string()
}

async fn serve_redirect(bind: SocketAddr, https_port: u16) -> Result<()> {
    let app: Router = Router::new()
        .fallback(any(redirect_handler))
        .with_state(https_port);
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .with_context(|| format!("bind admin HTTP redirect listener at {bind}"))?;
    axum::serve(listener, app)
        .await
        .context("admin HTTP redirect axum::serve")?;
    Ok(())
}

/// Build the 308 target URL. We trust the `Host` header for the
/// hostname (this listener only ever serves the admin panel, so the
/// security implications of trusting `Host` are limited to "the
/// browser redirects to a different name it already typed"). If the
/// header is missing or malformed we 400 — better than emitting an
/// `https://:8000/...` URL that the browser will refuse.
async fn redirect_handler(
    State(https_port): State<u16>,
    headers: HeaderMap,
    OriginalUri(uri): OriginalUri,
) -> Result<Redirect, (StatusCode, &'static str)> {
    let host_header = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .ok_or((StatusCode::BAD_REQUEST, "missing or invalid Host header"))?;
    let bare_host = strip_host_port(host_header);
    let path_and_query = uri.path_and_query().map(|pq| pq.as_str()).unwrap_or("/");
    let target = format!("https://{bare_host}:{https_port}{path_and_query}");
    Ok(Redirect::permanent(&target))
}

/// Strip any `:port` suffix from a `Host` header value, returning
/// the bare hostname/IP. Handles three cases:
///
/// * `example.com:8000` → `example.com`
/// * `[::1]:8000` → `[::1]` (IPv6 literal, keep brackets)
/// * `[::1]` / `example.com` → unchanged
///
/// Returning a `&str` borrow lets the caller `format!` the redirect
/// URL without a heap allocation per request.
fn strip_host_port(host_header: &str) -> &str {
    if host_header.starts_with('[') {
        // IPv6 literal: port (if any) follows the closing `]`.
        // Without `]:` we either have a bare bracketed address
        // (no port) or a malformed header — pass through either way.
        match host_header.rfind("]:") {
            Some(idx) => &host_header[..=idx],
            None => host_header,
        }
    } else {
        // Hostname or IPv4 — at most one `:`, separating port.
        host_header
            .split_once(':')
            .map(|(h, _)| h)
            .unwrap_or(host_header)
    }
}

#[cfg(test)]
mod tests {
    use super::strip_host_port;

    #[test]
    fn strips_port_from_hostname() {
        assert_eq!(strip_host_port("example.com:8000"), "example.com");
        assert_eq!(strip_host_port("example.com"), "example.com");
    }

    #[test]
    fn strips_port_from_ipv4() {
        assert_eq!(strip_host_port("127.0.0.1:8000"), "127.0.0.1");
        assert_eq!(strip_host_port("127.0.0.1"), "127.0.0.1");
    }

    #[test]
    fn preserves_ipv6_brackets() {
        // Bracketed IPv6, with and without a port. The brackets must
        // survive so the resulting redirect URL parses on the client.
        assert_eq!(strip_host_port("[::1]:8000"), "[::1]");
        assert_eq!(strip_host_port("[::1]"), "[::1]");
        assert_eq!(strip_host_port("[2001:db8::1]:443"), "[2001:db8::1]");
        assert_eq!(strip_host_port("[2001:db8::1]"), "[2001:db8::1]");
    }
}
