//! Per-source-IP rate limiting and auth-failure backoff.
//!
//! Two protections layered together:
//!
//!   * **Register rate cap** — at most `MAX_REGISTERS_PER_WINDOW`
//!     successful registration *attempts* per `REGISTER_WINDOW` per
//!     source IP. Prevents memory amplification from an attacker
//!     blasting registers at line rate and growing the registry /
//!     mpsc-channel allocations even on an open-mode server.
//!
//!   * **Auth-failure exponential backoff** — after a bad password,
//!     the source IP must wait an exponentially-growing delay before
//!     its next attempt is considered. Slows brute-force probing
//!     from millions of guesses/sec down to a handful per minute
//!     without locking out legitimate users (one mistype still gets
//!     them in 200 ms later).
//!
//! The backoff is keyed on IP, not on (IP, username), because the
//! username field is unauthenticated and an attacker would just
//! rotate it. IP-based throttling can be defeated by an attacker
//! with many source IPs (NAT / botnet), but that's an explicit
//! non-goal for this layer — defense in depth, not an ACL.
//!
//! State lives behind a `tokio::sync::Mutex` because both gRPC
//! handlers (`register`) and the optional eviction sweep (future
//! work) need to touch it. The lock is held only for the bookkeeping
//! check itself, never across awaits to the registry.

use std::collections::HashMap;
use std::net::IpAddr;
use std::time::{Duration, Instant};

use tokio::sync::Mutex;

/// Registers any single IP may complete in `REGISTER_WINDOW`. Five
/// per minute is comfortable for a real human reconnecting, intent on
/// trying different display names, etc., but well below what any
/// scripted attacker would want.
const MAX_REGISTERS_PER_WINDOW: u32 = 5;
const REGISTER_WINDOW: Duration = Duration::from_secs(60);

/// Identity-challenge nonces any single IP may request in
/// `CHALLENGE_WINDOW`. Looser than the register cap because a client
/// fetches a challenge once per connect (and the endpoint is cheaper than
/// register), but still bounds the amplification of an attacker hammering
/// it for free self-expiring blobs — 30/min caps a single-IP flood at
/// ~0.5/s instead of line rate. Tracked independently of the register
/// counter so a challenge never eats into the register budget.
const MAX_CHALLENGES_PER_WINDOW: u32 = 30;
const CHALLENGE_WINDOW: Duration = Duration::from_secs(60);

/// First auth-failure delay. Doubles on each consecutive failure
/// until capped at `MAX_BACKOFF`.
const BACKOFF_INITIAL: Duration = Duration::from_millis(200);
const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// Stale-entry eviction threshold. An IP that hasn't touched the
/// throttle in this long is forgotten on the next gate check. Keeps
/// the map bounded without a dedicated sweeper.
const IDLE_EVICTION: Duration = Duration::from_secs(600);

#[derive(Debug)]
struct Entry {
    /// Number of registers counted in the current window.
    register_count: u32,
    /// When the current window opened.
    register_window_start: Instant,
    /// Number of identity-challenge nonces issued in the current window.
    /// Independent of the register counter — fetching a challenge must
    /// not consume the (much tighter) register budget.
    challenge_count: u32,
    /// When the current challenge window opened.
    challenge_window_start: Instant,
    /// Consecutive auth failures since the last success.
    auth_failures: u32,
    /// Earliest instant the next register is allowed. `None` means
    /// "no backoff in effect".
    backoff_until: Option<Instant>,
    /// Last time we observed *any* activity from this IP. Used for
    /// the idle-eviction sweep.
    last_touched: Instant,
}

impl Entry {
    fn new(now: Instant) -> Self {
        Self {
            register_count: 0,
            register_window_start: now,
            challenge_count: 0,
            challenge_window_start: now,
            auth_failures: 0,
            backoff_until: None,
            last_touched: now,
        }
    }
}

/// Reason a `try_register` gate refused the request, mapped 1:1 to a
/// gRPC status by the caller.
#[derive(Debug, Clone, Copy)]
pub enum ThrottleReject {
    /// Too many registers from this IP in the current window.
    RateLimited,
    /// Backoff after recent auth failures hasn't elapsed yet.
    Backoff,
}

#[derive(Default)]
pub struct IpThrottle {
    inner: Mutex<HashMap<IpAddr, Entry>>,
}

impl IpThrottle {
    pub fn new() -> Self {
        Self::default()
    }

    /// Gate a fresh register attempt. Returns `Ok(())` to proceed, or
    /// `Err(ThrottleReject)` for the caller to translate into a gRPC
    /// status. Caller must follow up with [`record_auth_success`] or
    /// [`record_auth_failure`] once the password check resolves.
    pub async fn try_register(&self, ip: IpAddr) -> Result<(), ThrottleReject> {
        let now = Instant::now();
        let mut map = self.inner.lock().await;

        // Opportunistic prune: drop entries that haven't been touched
        // in IDLE_EVICTION. Cheap because the map is small (active-IP
        // count) and this only runs on register, not on hot paths.
        map.retain(|_, e| now.duration_since(e.last_touched) < IDLE_EVICTION);

        let entry = map.entry(ip).or_insert_with(|| Entry::new(now));
        entry.last_touched = now;

        // Backoff gate first — a banned IP shouldn't even count
        // toward the register window.
        if let Some(until) = entry.backoff_until {
            if now < until {
                return Err(ThrottleReject::Backoff);
            }
            entry.backoff_until = None;
        }

        // Slide the register window if it expired.
        if now.duration_since(entry.register_window_start) >= REGISTER_WINDOW {
            entry.register_window_start = now;
            entry.register_count = 0;
        }
        if entry.register_count >= MAX_REGISTERS_PER_WINDOW {
            return Err(ThrottleReject::RateLimited);
        }
        entry.register_count += 1;
        Ok(())
    }

    /// Gate an identity-challenge request. Independent per-IP cap on its
    /// own window — does **not** touch the register counter or the
    /// auth-failure backoff (issuing a stateless nonce is neither a
    /// register nor an authentication). Returns `Ok(())` to proceed or
    /// `Err(ThrottleReject::RateLimited)` once the IP exceeds
    /// `MAX_CHALLENGES_PER_WINDOW`.
    pub async fn try_challenge(&self, ip: IpAddr) -> Result<(), ThrottleReject> {
        let now = Instant::now();
        let mut map = self.inner.lock().await;

        // Same opportunistic idle prune as try_register — keeps the map
        // bounded to active IPs without a dedicated sweeper.
        map.retain(|_, e| now.duration_since(e.last_touched) < IDLE_EVICTION);

        let entry = map.entry(ip).or_insert_with(|| Entry::new(now));
        entry.last_touched = now;

        if now.duration_since(entry.challenge_window_start) >= CHALLENGE_WINDOW {
            entry.challenge_window_start = now;
            entry.challenge_count = 0;
        }
        if entry.challenge_count >= MAX_CHALLENGES_PER_WINDOW {
            return Err(ThrottleReject::RateLimited);
        }
        entry.challenge_count += 1;
        Ok(())
    }

    /// Mark a successful authentication for this IP — clears the
    /// failure counter and any in-flight backoff so the next attempt
    /// is unimpeded.
    pub async fn record_auth_success(&self, ip: IpAddr) {
        let mut map = self.inner.lock().await;
        if let Some(entry) = map.get_mut(&ip) {
            entry.auth_failures = 0;
            entry.backoff_until = None;
            entry.last_touched = Instant::now();
        }
    }

    /// Mark a failed authentication. Increments the failure counter
    /// and arms an exponentially-growing backoff window. The first
    /// failure delays the next attempt by `BACKOFF_INITIAL`; each
    /// subsequent failure doubles, capped at `MAX_BACKOFF`.
    pub async fn record_auth_failure(&self, ip: IpAddr) {
        let now = Instant::now();
        let mut map = self.inner.lock().await;
        let entry = map.entry(ip).or_insert_with(|| Entry::new(now));
        entry.last_touched = now;
        entry.auth_failures = entry.auth_failures.saturating_add(1);
        // 2^(n-1) * initial, capped. Saturating arithmetic so we
        // don't overflow on a sustained brute-force.
        let exponent = entry.auth_failures.saturating_sub(1).min(20);
        let backoff = BACKOFF_INITIAL
            .saturating_mul(1u32 << exponent)
            .min(MAX_BACKOFF);
        entry.backoff_until = Some(now + backoff);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn ip(n: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(127, 0, 0, n))
    }

    #[tokio::test]
    async fn allows_under_register_cap() {
        let t = IpThrottle::new();
        for _ in 0..MAX_REGISTERS_PER_WINDOW {
            t.try_register(ip(1)).await.unwrap();
        }
    }

    #[tokio::test]
    async fn rejects_over_register_cap() {
        let t = IpThrottle::new();
        for _ in 0..MAX_REGISTERS_PER_WINDOW {
            t.try_register(ip(1)).await.unwrap();
        }
        let err = t.try_register(ip(1)).await.unwrap_err();
        assert!(matches!(err, ThrottleReject::RateLimited));
    }

    #[tokio::test]
    async fn separate_ips_independent() {
        let t = IpThrottle::new();
        for _ in 0..MAX_REGISTERS_PER_WINDOW {
            t.try_register(ip(1)).await.unwrap();
        }
        // Different IP should not be affected.
        t.try_register(ip(2)).await.unwrap();
    }

    #[tokio::test]
    async fn auth_failure_arms_backoff() {
        let t = IpThrottle::new();
        t.try_register(ip(3)).await.unwrap();
        t.record_auth_failure(ip(3)).await;
        // The next attempt is inside the backoff window.
        let err = t.try_register(ip(3)).await.unwrap_err();
        assert!(matches!(err, ThrottleReject::Backoff));
    }

    #[tokio::test]
    async fn auth_success_clears_backoff() {
        let t = IpThrottle::new();
        t.try_register(ip(4)).await.unwrap();
        t.record_auth_failure(ip(4)).await;
        // Manually clear, simulating a delayed legitimate success
        // (we can't sleep through MAX_BACKOFF in a test).
        t.record_auth_success(ip(4)).await;
        t.try_register(ip(4)).await.unwrap();
    }

    #[tokio::test]
    async fn allows_under_challenge_cap_then_rejects() {
        let t = IpThrottle::new();
        for _ in 0..MAX_CHALLENGES_PER_WINDOW {
            t.try_challenge(ip(5)).await.unwrap();
        }
        let err = t.try_challenge(ip(5)).await.unwrap_err();
        assert!(matches!(err, ThrottleReject::RateLimited));
    }

    #[tokio::test]
    async fn challenge_cap_is_per_ip() {
        let t = IpThrottle::new();
        for _ in 0..MAX_CHALLENGES_PER_WINDOW {
            t.try_challenge(ip(6)).await.unwrap();
        }
        // A different IP has its own fresh budget.
        t.try_challenge(ip(7)).await.unwrap();
    }

    #[tokio::test]
    async fn challenge_and_register_budgets_are_independent() {
        let t = IpThrottle::new();
        // Exhaust the register budget for an IP.
        for _ in 0..MAX_REGISTERS_PER_WINDOW {
            t.try_register(ip(8)).await.unwrap();
        }
        assert!(matches!(
            t.try_register(ip(8)).await.unwrap_err(),
            ThrottleReject::RateLimited
        ));
        // Challenges from the same IP are unaffected — a connect's
        // challenge must not be collateral damage of the register cap,
        // and (conversely) challenges don't burn register slots.
        for _ in 0..MAX_CHALLENGES_PER_WINDOW {
            t.try_challenge(ip(8)).await.unwrap();
        }

        // And the reverse: exhausting challenges doesn't block registers
        // for a *fresh* IP's register budget.
        for _ in 0..MAX_CHALLENGES_PER_WINDOW {
            t.try_challenge(ip(9)).await.unwrap();
        }
        assert!(matches!(
            t.try_challenge(ip(9)).await.unwrap_err(),
            ThrottleReject::RateLimited
        ));
        t.try_register(ip(9)).await.unwrap();
    }

    #[tokio::test]
    async fn challenge_does_not_arm_or_observe_backoff() {
        let t = IpThrottle::new();
        // An auth failure arms the register backoff…
        t.try_register(ip(10)).await.unwrap();
        t.record_auth_failure(ip(10)).await;
        assert!(matches!(
            t.try_register(ip(10)).await.unwrap_err(),
            ThrottleReject::Backoff
        ));
        // …but the challenge gate ignores backoff entirely (it's not an
        // auth step), so the client can still fetch a fresh nonce.
        t.try_challenge(ip(10)).await.unwrap();
    }
}
