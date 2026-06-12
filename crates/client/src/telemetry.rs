//! Connection-quality telemetry: the client measures its own inbound
//! link health and publishes a lock-free readout for the UI + a periodic
//! report up to the server (for the admin dashboard).
//!
//! Three metrics, each measured where it's cheapest:
//!
//!   * **Loss** — the server→client audio/pong stream carries a strictly
//!     monotonic per-session sequence number. A gap between consecutive
//!     accepted seqs is dropped packets; we track received-vs-expected
//!     over a sliding window and report the percentage.
//!   * **Jitter** — the spacing between inbound audio packets should be
//!     the nominal 10 ms frame cadence. We track the mean absolute
//!     deviation of actual inter-arrival gaps from that nominal, smoothed
//!     (RFC 3550-style) — the "how bursty is my link" number.
//!   * **RTT** — the UDP keepalive carries a timestamped probe the server
//!     bounces back as a pong; the round-trip is `now - send_ts`, smoothed
//!     over successive probes.
//!
//! All updates happen on the audio recv / keepalive tasks; the UI and the
//! report task only read the published [`ConnQuality`] snapshot, so the
//! hot path stays mutex-free (same `Arc`-of-atomics shape as
//! `audio::AudioGains`).

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

/// Nominal inbound packet spacing: one 10 ms frame. Jitter is deviation
/// from this. (Pongs arrive off-cadence but are far rarer than audio and
/// only nudge the estimate; jitter is meaningful during a transmission,
/// which is exactly when audio packets flow at 10 ms.)
const NOMINAL_GAP_MS: f64 = 10.0;
/// Loss is computed over a sliding window of this many expected packets —
/// ~5 s of continuous audio at 100 packets/s. Long enough to be stable,
/// short enough to react to a degrading link within a few seconds.
const LOSS_WINDOW: u64 = 500;
/// EWMA weight for the smoothed jitter estimate (RFC 3550 uses 1/16).
const JITTER_ALPHA: f64 = 1.0 / 16.0;
/// EWMA weight for smoothed RTT — heavier smoothing than jitter since
/// probes are sparse (every ~3 s) and we want a steady readout.
const RTT_ALPHA: f64 = 0.25;

/// Published, lock-free quality readout. Cloneable `Arc` handle shared by
/// the measuring tasks (writers) and the UI + report task (readers).
#[derive(Clone)]
pub struct QualityHandle {
    rtt_ms: Arc<AtomicU32>,
    jitter_ms: Arc<AtomicU32>,
    loss_pct_centi: Arc<AtomicU32>,
    /// Bumped on every metric update; `0` means "nothing measured yet"
    /// so the UI can show a placeholder instead of a misleading 0/0/0.
    updates: Arc<AtomicU64>,
}

impl QualityHandle {
    fn new() -> Self {
        Self {
            rtt_ms: Arc::new(AtomicU32::new(0)),
            jitter_ms: Arc::new(AtomicU32::new(0)),
            loss_pct_centi: Arc::new(AtomicU32::new(0)),
            updates: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Current snapshot for the UI / report task.
    pub fn snapshot(&self) -> ConnQuality {
        ConnQuality {
            rtt_ms: self.rtt_ms.load(Ordering::Relaxed),
            jitter_ms: self.jitter_ms.load(Ordering::Relaxed),
            loss_pct_centi: self.loss_pct_centi.load(Ordering::Relaxed),
            fresh: self.updates.load(Ordering::Relaxed) > 0,
        }
    }

    fn publish_rtt(&self, ms: u32) {
        self.rtt_ms.store(ms, Ordering::Relaxed);
        self.updates.fetch_add(1, Ordering::Relaxed);
    }

    fn publish_jitter(&self, ms: u32) {
        self.jitter_ms.store(ms, Ordering::Relaxed);
        self.updates.fetch_add(1, Ordering::Relaxed);
    }

    fn publish_loss(&self, centi: u32) {
        self.loss_pct_centi.store(centi, Ordering::Relaxed);
        self.updates.fetch_add(1, Ordering::Relaxed);
    }
}

/// A plain snapshot of the published metrics. `fresh = false` until the
/// first measurement lands.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ConnQuality {
    pub rtt_ms: u32,
    pub jitter_ms: u32,
    pub loss_pct_centi: u32,
    pub fresh: bool,
}

impl ConnQuality {
    /// A coarse 0–4 "signal bars" score from the three metrics, for the
    /// client strip glyph. 4 = excellent, 0 = unusable. Takes the *worst*
    /// of the three sub-scores so any single bad dimension pulls the bars
    /// down — a 5 ms RTT doesn't excuse 30% loss. Returns `None` when no
    /// measurement has landed yet (UI shows a neutral placeholder).
    pub fn bars(&self) -> Option<u8> {
        if !self.fresh {
            return None;
        }
        // Thresholds picked for a voice link: each maps a metric to 0–4.
        let rtt = score(self.rtt_ms as f64, &[80.0, 150.0, 250.0, 400.0]);
        let jit = score(self.jitter_ms as f64, &[10.0, 25.0, 50.0, 90.0]);
        let loss = score(self.loss_pct_centi as f64, &[50.0, 200.0, 500.0, 1000.0]);
        Some(rtt.min(jit).min(loss))
    }
}

/// Map a metric to a 0–4 bar score against ascending thresholds: below
/// `t[0]` → 4 bars, …, at/above `t[3]` → 0 bars.
fn score(v: f64, t: &[f64; 4]) -> u8 {
    if v < t[0] {
        4
    } else if v < t[1] {
        3
    } else if v < t[2] {
        2
    } else if v < t[3] {
        1
    } else {
        0
    }
}

/// The stateful measurement engine, owned by the recv task. Feeds the
/// shared [`QualityHandle`].
pub struct QualityTracker {
    handle: QualityHandle,
    // ── Loss (S2C seq gaps over a sliding window) ──
    last_seq: Option<u64>,
    window_received: u64,
    window_expected: u64,
    // ── Jitter (inter-arrival deviation from 10 ms) ──
    last_arrival: Option<Instant>,
    jitter_ms: f64,
    jitter_seeded: bool,
    // ── RTT (smoothed pong round-trip) ──
    rtt_ms: f64,
    rtt_seeded: bool,
}

impl QualityTracker {
    pub fn new() -> (Self, QualityHandle) {
        let handle = QualityHandle::new();
        let tracker = Self {
            handle: handle.clone(),
            last_seq: None,
            window_received: 0,
            window_expected: 0,
            last_arrival: None,
            jitter_ms: 0.0,
            jitter_seeded: false,
            rtt_ms: 0.0,
            rtt_seeded: false,
        };
        (tracker, handle)
    }

    /// Record an accepted inbound audio packet with sequence `seq`,
    /// arriving at `at`. Drives both the loss and jitter estimators.
    /// Call this only for packets that pass the replay check (so a
    /// reordered/duplicate seq doesn't corrupt the window).
    pub fn on_audio(&mut self, seq: u64, at: Instant) {
        self.account_seq(seq);
        self.account_arrival(at);
    }

    /// Loss accounting: the expected count advances by the seq delta, the
    /// received count by 1. A delta >1 means we missed `delta - 1`
    /// packets. We never let a backwards/equal seq through here (the
    /// caller's replay check guarantees strictly increasing), so the
    /// window only grows monotonically until it rolls over.
    fn account_seq(&mut self, seq: u64) {
        if let Some(prev) = self.last_seq {
            let delta = seq.saturating_sub(prev);
            // delta is ≥1 by the strict-monotonic guarantee.
            self.window_expected += delta;
            self.window_received += 1;
        } else {
            // First packet of a stream seeds the baseline; it's neither
            // a loss nor a hit (nothing to compare against).
            self.window_expected += 1;
            self.window_received += 1;
        }
        self.last_seq = Some(seq);

        if self.window_expected >= LOSS_WINDOW {
            let lost = self.window_expected.saturating_sub(self.window_received);
            // percent ×100, rounded.
            let centi = ((lost as f64 / self.window_expected as f64) * 10_000.0).round() as u32;
            self.handle.publish_loss(centi.min(10_000));
            // Slide: halve the window so the estimate keeps reflecting
            // recent history without a hard reset to zero each time.
            self.window_expected /= 2;
            self.window_received /= 2;
        }
    }

    /// Jitter accounting (RFC 3550-style mean deviation): compare the
    /// actual gap since the last packet to the nominal 10 ms, and fold
    /// the absolute difference into a smoothed estimate. A burst (two
    /// packets back-to-back then a long gap) shows up as elevated
    /// jitter; a steady stream trends toward zero.
    fn account_arrival(&mut self, at: Instant) {
        if let Some(prev) = self.last_arrival {
            let gap_ms = at.saturating_duration_since(prev).as_secs_f64() * 1000.0;
            // Ignore the long silence between transmissions — a gap many
            // times the nominal is a PTT pause, not jitter. Only measure
            // within a plausible in-transmission spacing.
            if gap_ms < NOMINAL_GAP_MS * 8.0 {
                let dev = (gap_ms - NOMINAL_GAP_MS).abs();
                if self.jitter_seeded {
                    self.jitter_ms += (dev - self.jitter_ms) * JITTER_ALPHA;
                } else {
                    self.jitter_ms = dev;
                    self.jitter_seeded = true;
                }
                self.handle.publish_jitter(self.jitter_ms.round() as u32);
            }
        }
        self.last_arrival = Some(at);
    }

    /// Record a completed RTT measurement (a pong came back): `rtt_ms` is
    /// the raw round-trip; we publish a smoothed value.
    pub fn on_rtt(&mut self, rtt_ms: f64) {
        let rtt_ms = rtt_ms.max(0.0);
        if self.rtt_seeded {
            self.rtt_ms += (rtt_ms - self.rtt_ms) * RTT_ALPHA;
        } else {
            self.rtt_ms = rtt_ms;
            self.rtt_seeded = true;
        }
        self.handle.publish_rtt(self.rtt_ms.round() as u32);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn at(ms: u64) -> Instant {
        // A fixed base plus an offset — Instant has no public constructor,
        // so anchor on `now` and add. Monotonic within a test.
        BASE.get_or_init(Instant::now);
        *BASE.get().unwrap() + Duration::from_millis(ms)
    }
    use std::sync::OnceLock;
    static BASE: OnceLock<Instant> = OnceLock::new();

    #[test]
    fn clean_stream_reports_no_loss() {
        let (mut t, h) = QualityTracker::new();
        // Perfectly consecutive seqs at a steady 10 ms cadence.
        for i in 1..=LOSS_WINDOW + 10 {
            t.on_audio(i, at(i * 10));
        }
        let q = h.snapshot();
        assert!(q.fresh);
        assert_eq!(q.loss_pct_centi, 0, "no gaps → 0% loss");
    }

    #[test]
    fn dropped_packets_show_as_loss() {
        let (mut t, h) = QualityTracker::new();
        // Drop every 10th seq → ~10% loss.
        let mut clock = 0u64;
        let mut seq = 0u64;
        for i in 1..=LOSS_WINDOW + 50 {
            seq += 1;
            if i % 10 == 0 {
                seq += 1; // skip one — a gap the receiver never saw
            }
            clock += 10;
            t.on_audio(seq, at(clock));
        }
        let loss = h.snapshot().loss_pct_centi;
        // ~10% = 1000 centi; allow generous slack for the window math.
        assert!(
            (700..=1300).contains(&loss),
            "expected ~10% loss, got {loss} centi-pct"
        );
    }

    #[test]
    fn steady_cadence_has_low_jitter() {
        let (mut t, h) = QualityTracker::new();
        for i in 1..=200 {
            t.on_audio(i, at(i * 10)); // exactly 10 ms apart
        }
        assert!(
            h.snapshot().jitter_ms <= 1,
            "metronomic stream should have ~0 jitter, got {}",
            h.snapshot().jitter_ms
        );
    }

    #[test]
    fn bursty_cadence_has_high_jitter() {
        let (mut t, h) = QualityTracker::new();
        // Alternate 2 ms and 18 ms gaps — same average, lots of jitter.
        let mut clock = 0u64;
        for i in 1..=400 {
            clock += if i % 2 == 0 { 2 } else { 18 };
            t.on_audio(i, at(clock));
        }
        assert!(
            h.snapshot().jitter_ms >= 5,
            "bursty stream should register jitter, got {}",
            h.snapshot().jitter_ms
        );
    }

    #[test]
    fn ptt_pause_gap_is_ignored_not_counted_as_jitter() {
        // A long inter-arrival gap (a PTT pause / new transmission) is
        // outside the in-transmission spacing window, so it must not
        // register as jitter — otherwise every key-up→key-down would
        // spike the estimate.
        let (mut t, _h) = QualityTracker::new();
        t.on_audio(1, at(0));
        t.on_audio(2, at(10));
        let before = t.jitter_ms;
        // 5 seconds later — far beyond the nominal spacing window.
        t.on_audio(3, at(5_000));
        assert_eq!(
            t.jitter_ms, before,
            "a multi-second pause must not spike jitter"
        );
    }

    #[test]
    fn rtt_smooths_toward_samples() {
        let (mut t, h) = QualityTracker::new();
        t.on_rtt(100.0);
        assert_eq!(h.snapshot().rtt_ms, 100);
        // Successive samples at 50 ms pull the smoothed value down but
        // not instantly (EWMA).
        for _ in 0..20 {
            t.on_rtt(50.0);
        }
        let rtt = h.snapshot().rtt_ms;
        assert!(
            (50..=60).contains(&rtt),
            "RTT should converge near 50, got {rtt}"
        );
    }

    #[test]
    fn bars_scale_with_quality() {
        // Pristine link → 4 bars.
        let good = ConnQuality {
            rtt_ms: 20,
            jitter_ms: 2,
            loss_pct_centi: 0,
            fresh: true,
        };
        assert_eq!(good.bars(), Some(4));
        // One bad dimension (heavy loss) drags the whole score down.
        let lossy = ConnQuality {
            rtt_ms: 20,
            jitter_ms: 2,
            loss_pct_centi: 1500, // 15%
            fresh: true,
        };
        assert_eq!(lossy.bars(), Some(0));
        // Unmeasured → no bars.
        assert_eq!(ConnQuality::default().bars(), None);
    }
}
