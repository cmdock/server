//! Circuit breaker for external service calls (LLM API).
//!
//! Tracks failure rate and automatically opens the circuit when failures
//! exceed a threshold, preventing wasted time on calls that will likely fail.
//!
//! States:
//!   Closed  — normal operation, calls pass through
//!   Open    — circuit tripped, calls fail fast (return error immediately)
//!   HalfOpen — after cooldown, allow one probe call to test recovery

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

/// Circuit breaker configuration.
const FAILURE_THRESHOLD: u64 = 3; // Open after 3 consecutive failures
const COOLDOWN: Duration = Duration::from_secs(60); // Stay open for 60s
const WINDOW: Duration = Duration::from_secs(30); // Reset counters after 30s of success

#[derive(Debug, Clone, Copy, PartialEq)]
enum State {
    Closed,
    Open,
    HalfOpen,
}

pub struct CircuitBreaker {
    state: RwLock<CircuitState>,
    total_trips: AtomicU64,
}

struct CircuitState {
    state: State,
    consecutive_failures: u64,
    last_failure: Option<Instant>,
    opened_at: Option<Instant>,
    last_success: Option<Instant>,
}

impl Default for CircuitBreaker {
    fn default() -> Self {
        Self::new()
    }
}

impl CircuitBreaker {
    pub fn new() -> Self {
        Self {
            state: RwLock::new(CircuitState {
                state: State::Closed,
                consecutive_failures: 0,
                last_failure: None,
                opened_at: None,
                last_success: None,
            }),
            total_trips: AtomicU64::new(0),
        }
    }

    /// Check if a call should be allowed.
    /// Returns Ok(()) if allowed, Err(reason) if circuit is open.
    pub async fn check(&self) -> Result<(), String> {
        let mut state = self.state.write().await;

        match state.state {
            State::Closed => Ok(()),
            State::Open => {
                // Check if cooldown has elapsed
                if let Some(opened_at) = state.opened_at {
                    if opened_at.elapsed() >= COOLDOWN {
                        state.state = State::HalfOpen;
                        tracing::info!("LLM circuit breaker: Open → HalfOpen (probing)");
                        return Ok(());
                    }
                }
                Err(format!(
                    "Circuit open — {} consecutive failures, cooldown {}s remaining",
                    state.consecutive_failures,
                    COOLDOWN
                        .checked_sub(
                            state
                                .opened_at
                                .map(|t| t.elapsed())
                                .unwrap_or(Duration::ZERO)
                        )
                        .unwrap_or(Duration::ZERO)
                        .as_secs()
                ))
            }
            State::HalfOpen => {
                // Allow one probe call
                Ok(())
            }
        }
    }

    /// Record a successful call.
    pub async fn record_success(&self) {
        let mut state = self.state.write().await;
        state.consecutive_failures = 0;
        state.last_success = Some(Instant::now());

        if state.state != State::Closed {
            tracing::info!("LLM circuit breaker: {:?} → Closed (success)", state.state);
            state.state = State::Closed;
            state.opened_at = None;
        }
    }

    /// Record a failed call.
    pub async fn record_failure(&self) {
        let mut state = self.state.write().await;
        state.consecutive_failures += 1;
        state.last_failure = Some(Instant::now());

        // Reset failure count if last success was within the window
        if let Some(last_success) = state.last_success {
            if last_success.elapsed() < WINDOW {
                // Recent success — don't trip yet
                return;
            }
        }

        if state.consecutive_failures >= FAILURE_THRESHOLD && state.state == State::Closed {
            state.state = State::Open;
            state.opened_at = Some(Instant::now());
            self.total_trips.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(
                "LLM circuit breaker: Closed → Open (after {} failures)",
                state.consecutive_failures
            );
        }

        // HalfOpen probe failed — reopen
        if state.state == State::HalfOpen {
            state.state = State::Open;
            state.opened_at = Some(Instant::now());
            tracing::warn!("LLM circuit breaker: HalfOpen → Open (probe failed)");
        }
    }

    /// Force the circuit into Open state with an `opened_at` time in the past.
    /// Used by tests to simulate cooldown expiry without real sleeps.
    #[cfg(test)]
    async fn force_open_with_elapsed_cooldown(&self) {
        let mut state = self.state.write().await;
        state.state = State::Open;
        state.consecutive_failures = FAILURE_THRESHOLD;
        // Set opened_at far enough in the past that cooldown has elapsed
        state.opened_at = Some(Instant::now() - COOLDOWN - Duration::from_secs(1));
    }

    /// Get current status as a string (for admin endpoint).
    pub fn status(&self) -> String {
        // Use try_read to avoid blocking — worst case we report "unknown"
        match self.state.try_read() {
            Ok(state) => match state.state {
                State::Closed => "closed".to_string(),
                State::Open => format!(
                    "open (failures: {}, trips: {})",
                    state.consecutive_failures,
                    self.total_trips.load(Ordering::Relaxed)
                ),
                State::HalfOpen => "half-open (probing)".to_string(),
            },
            Err(_) => "unknown (lock contended)".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_initial_state_is_closed() {
        let cb = CircuitBreaker::new();
        assert!(
            cb.check().await.is_ok(),
            "new circuit breaker should allow calls"
        );
        assert!(cb.status().contains("closed"));
    }

    #[tokio::test]
    async fn test_opens_after_consecutive_failures() {
        let cb = CircuitBreaker::new();

        // Record failures up to threshold (no recent success, so WINDOW check won't reset)
        for _ in 0..FAILURE_THRESHOLD {
            cb.record_failure().await;
        }

        // Circuit should now be open
        let result = cb.check().await;
        assert!(
            result.is_err(),
            "circuit should be open after {FAILURE_THRESHOLD} failures"
        );
        assert!(
            result.unwrap_err().contains("Circuit open"),
            "error message should indicate open circuit"
        );
        assert_eq!(cb.total_trips.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn test_success_resets_failure_count() {
        let cb = CircuitBreaker::new();

        // Record some failures (but not enough to trip)
        for _ in 0..FAILURE_THRESHOLD - 1 {
            cb.record_failure().await;
        }

        // Success resets the counter
        cb.record_success().await;

        // Now even more failures shouldn't trip immediately (counter was reset)
        for _ in 0..FAILURE_THRESHOLD - 1 {
            cb.record_failure().await;
        }

        assert!(
            cb.check().await.is_ok(),
            "circuit should still be closed after reset + partial failures"
        );
    }

    #[tokio::test]
    async fn test_half_open_after_cooldown() {
        let cb = CircuitBreaker::new();

        // Force into Open state with cooldown already elapsed
        cb.force_open_with_elapsed_cooldown().await;

        // check() should transition to HalfOpen and allow the call
        let result = cb.check().await;
        assert!(
            result.is_ok(),
            "should transition to HalfOpen after cooldown"
        );

        // Verify state is HalfOpen
        let state = cb.state.read().await;
        assert_eq!(state.state, State::HalfOpen);
    }

    #[tokio::test]
    async fn test_closes_on_success_after_half_open() {
        let cb = CircuitBreaker::new();

        // Force into Open with expired cooldown, then check to transition to HalfOpen
        cb.force_open_with_elapsed_cooldown().await;
        cb.check().await.unwrap(); // transitions to HalfOpen

        // Record success — should close the circuit
        cb.record_success().await;

        assert!(
            cb.check().await.is_ok(),
            "circuit should be closed after success in HalfOpen"
        );
        assert!(cb.status().contains("closed"));
    }

    #[tokio::test]
    async fn test_half_open_failure_reopens() {
        let cb = CircuitBreaker::new();

        // Force into Open with expired cooldown, transition to HalfOpen
        cb.force_open_with_elapsed_cooldown().await;
        cb.check().await.unwrap(); // transitions to HalfOpen

        // Probe fails — should reopen
        cb.record_failure().await;

        let result = cb.check().await;
        assert!(
            result.is_err(),
            "circuit should reopen after HalfOpen probe failure"
        );
    }

    #[tokio::test]
    async fn test_failures_within_success_window_dont_trip() {
        let cb = CircuitBreaker::new();

        // Record a success first (sets last_success)
        cb.record_success().await;

        // Record failures — but last_success is within WINDOW, so they shouldn't trip
        for _ in 0..FAILURE_THRESHOLD + 2 {
            cb.record_failure().await;
        }

        // Circuit should still be closed because of the recent success within WINDOW
        assert!(
            cb.check().await.is_ok(),
            "failures within success window should not trip the circuit"
        );
    }
}
