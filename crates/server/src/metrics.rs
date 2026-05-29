//! Server metrics: voice-relay bandwidth counters, a periodic sampler
//! that persists a 1-minute time-series, and host-health snapshots.
//!
//! - [`ByteCounters`] are cumulative atomics bumped by the UDP audio
//!   task on every packet (ingress on recv, egress on send). The sampler
//!   reads deltas to derive throughput — so a single counter pair feeds
//!   the whole bandwidth chart without per-sample bookkeeping in the hot
//!   path.
//! - [`run_sampler`] refreshes [`Health`] every few seconds (cheap reads
//!   for the dashboard) and writes one `metrics_samples` row per minute,
//!   pruning rows past the retention window. Only voice (UDP) traffic is
//!   counted; gRPC signaling is negligible beside audio.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::admin::db::AdminDb;
use crate::state::SharedRegistry;

/// Cumulative voice-relay byte counters. Incremented by the UDP audio
/// relay; read as deltas by the sampler.
#[derive(Default)]
pub struct ByteCounters {
    /// Ingress: bytes received from clients (`recv_from`).
    pub rx: AtomicU64,
    /// Egress: bytes relayed to clients (`send_to`).
    pub tx: AtomicU64,
}

impl ByteCounters {
    #[inline]
    pub fn add_rx(&self, n: u64) {
        self.rx.fetch_add(n, Ordering::Relaxed);
    }
    #[inline]
    pub fn add_tx(&self, n: u64) {
        self.tx.fetch_add(n, Ordering::Relaxed);
    }
}

pub type SharedByteCounters = Arc<ByteCounters>;

pub fn shared_counters() -> SharedByteCounters {
    Arc::new(ByteCounters::default())
}

/// Latest host-health snapshot, refreshed by [`run_sampler`].
#[derive(Clone, Copy, Default)]
pub struct Health {
    pub cpu_percent: f32,
    pub mem_used: u64,
    pub mem_total: u64,
    pub disk_used: u64,
    pub disk_total: u64,
}

pub type SharedHealth = Arc<Mutex<Health>>;

pub fn shared_health() -> SharedHealth {
    Arc::new(Mutex::new(Health::default()))
}

/// Read the current health snapshot (cheap clone; lock held briefly).
pub fn health_snapshot(health: &SharedHealth) -> Health {
    health.lock().map(|h| *h).unwrap_or_default()
}

/// How often host health is refreshed. Also the sampler's loop tick.
const HEALTH_INTERVAL: Duration = Duration::from_secs(5);
/// How often a persisted time-series row is written.
const SAMPLE_INTERVAL_SECS: f64 = 60.0;
/// Time-series retention (matches the panel's longest window + margin).
const RETENTION_SECS: i64 = 7 * 24 * 3600;
/// Audit-log retention.
const AUDIT_RETENTION_SECS: i64 = 30 * 24 * 3600;

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Background loop: refresh host health every [`HEALTH_INTERVAL`] and
/// write a persisted metrics row every [`SAMPLE_INTERVAL_SECS`], pruning
/// rows past retention. Runs for the life of the admin task.
pub async fn run_sampler(
    counters: SharedByteCounters,
    registry: SharedRegistry,
    db: AdminDb,
    health: SharedHealth,
    disk_anchor: PathBuf,
) {
    let mut sys = sysinfo::System::new();
    let mut last_rx = counters.rx.load(Ordering::Relaxed);
    let mut last_tx = counters.tx.load(Ordering::Relaxed);
    let mut last_sample = Instant::now();
    let mut ticker = tokio::time::interval(HEALTH_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        ticker.tick().await;

        // ── Host health (cheap, every tick) ───────────────────────
        sys.refresh_cpu_usage();
        sys.refresh_memory();
        let snapshot = Health {
            // First reading is ~0 until a second refresh lands; ticks are
            // seconds apart so it's accurate from the second tick on.
            cpu_percent: sys.global_cpu_usage(),
            mem_used: sys.used_memory(),
            mem_total: sys.total_memory(),
            ..disk_health(&disk_anchor)
        };
        if let Ok(mut h) = health.lock() {
            *h = snapshot;
        }

        // ── Persisted time-series row (every minute) ──────────────
        let elapsed = last_sample.elapsed().as_secs_f64();
        if elapsed >= SAMPLE_INTERVAL_SECS {
            let rx = counters.rx.load(Ordering::Relaxed);
            let tx = counters.tx.load(Ordering::Relaxed);
            let rx_bps = (rx.saturating_sub(last_rx) as f64 / elapsed).round() as u64;
            let tx_bps = (tx.saturating_sub(last_tx) as f64 / elapsed).round() as u64;
            last_rx = rx;
            last_tx = tx;
            last_sample = Instant::now();

            let (users, transmitting) = {
                let r = registry.lock().await;
                let users = r.clients.len() as u32;
                let transmitting = r
                    .rooms
                    .values()
                    .filter(|room| room.holder.is_some())
                    .count() as u32;
                (users, transmitting)
            };
            let ts = now_unix();
            if let Err(e) = db
                .insert_metric_sample(ts, rx_bps, tx_bps, users, transmitting)
                .await
            {
                tracing::warn!(error = ?e, "metrics sample insert failed");
            }
            let _ = db.prune_metrics(ts - RETENTION_SECS).await;
            let _ = db.prune_audit(ts - AUDIT_RETENTION_SECS).await;
        }
    }
}

/// Disk usage for the filesystem backing `anchor` (the data dir): the
/// mounted disk whose mount-point is the longest prefix of `anchor`,
/// else the largest disk. Returned as a partial [`Health`] so the caller
/// can spread it with `..`. `(0, 0)` when nothing matches.
fn disk_health(anchor: &Path) -> Health {
    let disks = sysinfo::Disks::new_with_refreshed_list();
    let anchor = anchor
        .canonicalize()
        .unwrap_or_else(|_| anchor.to_path_buf());
    let mut best: Option<(&sysinfo::Disk, usize)> = None;
    for d in disks.list() {
        let mp = d.mount_point();
        if anchor.starts_with(mp) {
            let len = mp.as_os_str().len();
            if best.map(|(_, l)| len > l).unwrap_or(true) {
                best = Some((d, len));
            }
        }
    }
    let disk = best
        .map(|(d, _)| d)
        .or_else(|| disks.list().iter().max_by_key(|d| d.total_space()));
    match disk {
        Some(d) => {
            let total = d.total_space();
            let avail = d.available_space();
            Health {
                disk_used: total.saturating_sub(avail),
                disk_total: total,
                ..Health::default()
            }
        }
        None => Health::default(),
    }
}
