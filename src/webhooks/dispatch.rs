//! Bounded async webhook dispatch (#149 introduced it off the response path;
//! #156 bounded it).
//!
//! `finalize_success` spawns webhook target-lookup + delivery off the
//! synchronous mutation response path — `deliver` retries inline (1s/10s/60s),
//! so a slow/dead endpoint must not stall the HTTP response. #156 caps the
//! number of concurrent in-flight dispatches so a burst to dead hooks cannot
//! pile up unbounded tasks.
//!
//! **Single source of truth: one `Semaphore`** (ADR-0002 §Independence — don't
//! complect a separate in-flight counter with a separate bound; the permits
//! ARE the in-flight set):
//! - admission control → `try_acquire_owned()` (no permit ⇒ caller SHEDs);
//! - in-flight count → `in_flight()` = `capacity - available_permits()`. (The
//!   `webhook_dispatch_in_flight` *gauge* metric is a separate accumulator,
//!   incremented on admit and decremented on guard-drop; it tracks the same
//!   quantity but is maintained by the metrics sink, not read off the permits.)
//! - quiescence (tests) → `acquire_many(capacity)` resolves only once every
//!   dispatch has finished.
//!
//! A shed is a **permanent drop** (no retry), so `capacity` is biased HIGH
//! (hundreds): it must bound pathological pile-up (a dead hook holds a permit
//! for the full ~71s retry budget) WITHOUT false-shedding a legitimate burst (a
//! large sync emits dozens of events that each hold a permit for only ms). Idle
//! dispatch tasks are cheap (~KB), so erring high is correct.
//!
//! **Known limitation — global cap is noisy-neighbour:** one user's dead-hook
//! storm can exhaust the shared cap and shed *other* users' deliveries until the
//! hook auto-disables (10 consecutive failures, per webhook-contract). High
//! capacity + auto-disable bound the window; per-user capping would fix it but
//! is out of #156's scope (#156 = bound the transport).

use std::sync::Arc;
use std::time::Duration;

use metrics::gauge;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Default max concurrent in-flight webhook dispatches. Biased high — bounds
/// pathological pile-up, not legitimate burst concurrency. Override with
/// `CMDOCK_WEBHOOK_DISPATCH_MAX_INFLIGHT` (positive integer; else default).
const DEFAULT_MAX_INFLIGHT: usize = 512;

/// Upper bound on capacity. `Semaphore::acquire_many` takes a `u32`, and
/// `await_quiescent` requests `capacity` permits at once, so capacity must fit
/// in a `u32` or quiescence could acquire fewer than all permits and report
/// idle while dispatches are still in flight. (No real deployment approaches
/// this — it guards against a fat-fingered env override.)
const MAX_INFLIGHT_CEILING: usize = u32::MAX as usize;

/// Read the dispatch capacity from the environment, falling back to the default.
/// Values are clamped to `1..=MAX_INFLIGHT_CEILING`; a zero / non-numeric /
/// out-of-range override is ignored or clamped rather than producing an invalid
/// tracker.
pub fn capacity_from_env() -> usize {
    std::env::var("CMDOCK_WEBHOOK_DISPATCH_MAX_INFLIGHT")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n >= 1)
        .map(|n| n.min(MAX_INFLIGHT_CEILING))
        .unwrap_or(DEFAULT_MAX_INFLIGHT)
}

/// Bounds AND observes concurrent webhook dispatches via a single semaphore.
pub struct WebhookDispatchTracker {
    permits: Arc<Semaphore>,
    capacity: usize,
}

impl WebhookDispatchTracker {
    /// `capacity` must be in `1..=MAX_INFLIGHT_CEILING` (a `u32`-fitting,
    /// non-zero count). Zero would shed every event forever and make
    /// `await_quiescent` trivially succeed; above `u32::MAX` would break the
    /// `acquire_many` quiescence request. `capacity_from_env` already clamps to
    /// this range — the assert pins the invariant for direct (test) callers.
    pub fn new(capacity: usize) -> Self {
        assert!(
            (1..=MAX_INFLIGHT_CEILING).contains(&capacity),
            "webhook dispatch capacity must be in 1..={MAX_INFLIGHT_CEILING}, got {capacity}",
        );
        Self {
            permits: Arc::new(Semaphore::new(capacity)),
            capacity,
        }
    }

    /// Maximum concurrent in-flight dispatches (the configured cap).
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Admit one dispatch if under capacity. `Some(guard)` ⇒ the caller spawns
    /// the dispatch with the guard moved into the future (the permit is released
    /// and the gauge decremented when that future finishes OR panics). `None` ⇒
    /// at capacity; the caller MUST shed the event (drop it + increment
    /// `webhook_dispatch_shed_total`).
    ///
    /// MUST be called before `tokio::spawn` so that once the mutation response
    /// returns, the in-flight count already reflects this pending dispatch.
    pub fn try_enter(&self) -> Option<WebhookDispatchGuard> {
        let permit = Arc::clone(&self.permits).try_acquire_owned().ok()?;
        gauge!("webhook_dispatch_in_flight").increment(1.0);
        Some(WebhookDispatchGuard { _permit: permit })
    }

    /// Current in-flight dispatch count (permits held).
    pub fn in_flight(&self) -> usize {
        self.capacity - self.permits.available_permits()
    }

    /// Wait until no dispatch is in flight (all permits free), or `timeout`
    /// elapses. Returns `true` if quiescent. Test-only signal — assumes no
    /// concurrent admission while waiting, which holds for its post-mutation
    /// test use (`try_acquire` bypasses the wait queue, so a concurrent admit
    /// could otherwise starve `acquire_many`).
    pub async fn await_quiescent(&self, timeout: Duration) -> bool {
        matches!(
            tokio::time::timeout(timeout, self.permits.acquire_many(self.capacity as u32)).await,
            Ok(Ok(_permit))
        )
    }
}

/// Releases the dispatch permit (and decrements the gauge) on drop — i.e. when
/// the spawned dispatch future finishes or panics. Move it into that future.
pub struct WebhookDispatchGuard {
    _permit: OwnedSemaphorePermit,
}

impl Drop for WebhookDispatchGuard {
    fn drop(&mut self) {
        gauge!("webhook_dispatch_in_flight").decrement(1.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn try_enter_bounds_at_capacity_and_releases_on_drop() {
        let t = WebhookDispatchTracker::new(2);
        let g1 = t.try_enter().expect("permit 1");
        let _g2 = t.try_enter().expect("permit 2");
        assert_eq!(t.in_flight(), 2);
        assert!(
            t.try_enter().is_none(),
            "third admit must shed at capacity 2"
        );
        drop(g1);
        assert_eq!(t.in_flight(), 1);
        let _g3 = t.try_enter().expect("permit freed after drop is reusable");
        assert_eq!(t.in_flight(), 2);
    }

    #[tokio::test]
    async fn await_quiescent_reflects_held_permits() {
        let t = WebhookDispatchTracker::new(4);
        assert!(
            t.await_quiescent(Duration::from_millis(50)).await,
            "idle ⇒ quiescent"
        );
        let g = t.try_enter().expect("permit");
        assert!(
            !t.await_quiescent(Duration::from_millis(50)).await,
            "a held permit blocks quiescence"
        );
        drop(g);
        assert!(
            t.await_quiescent(Duration::from_millis(50)).await,
            "released ⇒ quiescent again"
        );
    }

    #[test]
    #[should_panic(expected = "capacity must be in")]
    fn zero_capacity_is_rejected() {
        // Capacity 0 would shed every event forever and make await_quiescent
        // trivially succeed — disallowed by construction.
        let _ = WebhookDispatchTracker::new(0);
    }

    #[test]
    fn inflight_ceiling_fits_u32() {
        // The ceiling that `capacity_from_env`/`new` clamp to MUST fit u32 so
        // `await_quiescent`'s `acquire_many(capacity as u32)` can request every
        // permit (a larger cap would silently under-acquire and report idle
        // while dispatches are still in flight). This pins the constant; the
        // env clamping/assert that consume it are covered by `new`'s validation
        // and `zero_capacity_is_rejected`. (No env mutation — keeps the test
        // safe under the parallel test runner.)
        assert!(MAX_INFLIGHT_CEILING <= u32::MAX as usize);
    }
}
