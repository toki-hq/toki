//! Audit-log event pipeline.
//!
//! Producers across the server (signaling, the reaper, the admin RPCs,
//! the login endpoint) record events via [`record`] on a shared
//! [`AuditSink`] (an unbounded mpsc sender). A single [`run_writer`] task
//! — owned by the admin module, which holds the sqlite handle — drains
//! the channel and persists each event. Routing everything through one
//! writer keeps sqlite writes off every hot path and serializes them
//! without a shared lock.
//!
//! Events are fire-and-forget: if the writer has gone away the send is
//! dropped silently (auditing must never block or fail a request).

use tokio::sync::mpsc;

use crate::admin::db::{now_unix, AdminDb};

/// A pending audit entry. Timestamp is stamped by the writer so all rows
/// share one clock source.
#[derive(Debug, Clone)]
pub struct AuditEvent {
    pub kind: String,
    pub actor: String,
    pub frequency: String,
    pub detail: String,
}

pub type AuditSink = mpsc::UnboundedSender<AuditEvent>;

/// Build the producer/consumer pair. The sink is cloned to every
/// producer; the receiver is handed to [`run_writer`].
pub fn channel() -> (AuditSink, mpsc::UnboundedReceiver<AuditEvent>) {
    mpsc::unbounded_channel()
}

/// Fire-and-forget record. Never blocks; drops silently if the writer
/// task has exited (e.g. during shutdown).
pub fn record(sink: &AuditSink, kind: &str, actor: &str, frequency: &str, detail: &str) {
    let _ = sink.send(AuditEvent {
        kind: kind.to_string(),
        actor: actor.to_string(),
        frequency: frequency.to_string(),
        detail: detail.to_string(),
    });
}

/// Drain the channel into sqlite for the life of the process.
pub async fn run_writer(mut rx: mpsc::UnboundedReceiver<AuditEvent>, db: AdminDb) {
    while let Some(ev) = rx.recv().await {
        if let Err(e) = db
            .insert_audit(now_unix(), &ev.kind, &ev.actor, &ev.frequency, &ev.detail)
            .await
        {
            tracing::warn!(error = ?e, kind = %ev.kind, "audit insert failed");
        }
    }
}

// ── Kind vocabulary + filter buckets ──────────────────────────────
// Stored `kind` strings, grouped to back the `AuditFilter` categories.
// These are the ONLY values the recorder emits, so the gRPC layer can
// interpolate them into a `kind IN (...)` clause safely.

pub const KINDS_ADMIN: &[&str] = &[
    "kick",
    "move",
    "rename",
    "priority",
    "channel-name",
    "channel-clear",
    "server-config",
    "admin-password",
];
pub const KINDS_CONNECTIONS: &[&str] = &["connect", "disconnect"];
pub const KINDS_SECURITY: &[&str] = &["auth-ok", "auth-fail"];

pub const SYSTEM_ACTOR: &str = "SYSTEM";
