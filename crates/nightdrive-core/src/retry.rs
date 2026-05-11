//! Generic async retry with exponential backoff + jitter.
//!
//! Every external HTTP call in nightdrive eventually wraps through here so
//! transient infrastructure failures (Ollama warming up, SDXL OOM-and-restart,
//! YouTube 5xx, audio sidecar reload) don't take down a whole batch. The
//! caller supplies the work as an async closure and a predicate that decides
//! per-error whether to retry or bubble.
//!
//! ## Backoff schedule
//!
//! Default is exponential `1s → 2s → 4s → ...` capped at `max_attempts`
//! attempts (default 3 = 1 initial + 2 retries). Jitter is ±20% to avoid
//! thundering-herd reconnects when many tracks fail at once.
//!
//! ## Why not use one of the crates
//!
//! `tokio-retry`, `backoff`, `retry`, etc. each pull in ~5-10 transitive
//! deps for a 60-line utility. Hand-rolling here keeps the workspace tight
//! and the surface obvious for honesty-auditor reads. Cribbed from the
//! retry loop we open-coded in `nightdrive-llm::OpenclawLlm::generate_spec`;
//! that crate's loop will eventually call through here for symmetry.

use std::future::Future;
use std::time::Duration;
use tracing::{debug, warn};

/// Retry policy. Built with sensible nightdrive-shaped defaults; override per
/// caller as needed.
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub initial_backoff: Duration,
    /// Cap on each individual sleep — without this, an `exponential` schedule
    /// at attempt 10 sleeps for ~17 minutes, which is rarely what you want.
    pub max_backoff: Duration,
    /// Jitter as a fraction of the computed sleep. 0.2 = ±20%.
    pub jitter: f32,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            initial_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(30),
            jitter: 0.2,
        }
    }
}

impl RetryPolicy {
    /// Sleep duration for attempt `n` (1-indexed). `n=1` returns
    /// `initial_backoff`, `n=2` returns `initial_backoff * 2`, etc., capped at
    /// `max_backoff`.
    fn backoff_for(&self, n: u32) -> Duration {
        let base = self.initial_backoff.saturating_mul(2u32.saturating_pow(n.saturating_sub(1)));
        if base > self.max_backoff {
            self.max_backoff
        } else {
            base
        }
    }

    fn jittered(&self, base: Duration) -> Duration {
        if self.jitter <= 0.0 {
            return base;
        }
        // Cheap deterministic pseudo-jitter: use the current monotonic ns as
        // source. Doesn't need to be cryptographic — we just want to break
        // herd patterns across concurrent retries.
        let now_ns = std::time::Instant::now().elapsed().as_nanos() as u64;
        let span = (base.as_nanos() as f32 * self.jitter) as u64;
        if span == 0 {
            return base;
        }
        let delta = (now_ns % (span * 2)) as i64 - span as i64;
        let base_ns = base.as_nanos() as i64;
        let result = (base_ns + delta).max(0) as u64;
        Duration::from_nanos(result)
    }
}

/// Run `op` with `policy`, retrying as long as `should_retry(err)` returns
/// true and we haven't exhausted the attempt budget. Returns the operation's
/// success value or the last error.
///
/// `op` is called fresh per attempt (returns a new future each time), so it
/// can be a closure that constructs a new request body, opens a new file
/// handle, etc.
///
/// ```rust,ignore
/// use nightdrive_core::retry::{with_backoff, RetryPolicy};
///
/// let result = with_backoff(
///     RetryPolicy::default(),
///     || async {
///         http.get("https://flaky.example/health").send().await
///     },
///     |err: &reqwest::Error| err.is_timeout() || err.is_connect(),
/// ).await;
/// ```
pub async fn with_backoff<T, E, F, Fut, R>(
    policy: RetryPolicy,
    mut op: F,
    should_retry: R,
) -> Result<T, E>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, E>>,
    R: Fn(&E) -> bool,
    E: std::fmt::Display,
{
    let mut last_err: Option<E> = None;
    for attempt in 1..=policy.max_attempts {
        match op().await {
            Ok(value) => {
                if attempt > 1 {
                    debug!(attempt, "retry succeeded");
                }
                return Ok(value);
            }
            Err(e) if attempt < policy.max_attempts && should_retry(&e) => {
                let sleep_for = policy.jittered(policy.backoff_for(attempt));
                warn!(
                    attempt,
                    max = policy.max_attempts,
                    sleep_ms = sleep_for.as_millis() as u64,
                    error = %e,
                    "transient failure, will retry"
                );
                last_err = Some(e);
                tokio::time::sleep(sleep_for).await;
            }
            Err(e) => {
                if attempt >= policy.max_attempts {
                    warn!(attempt, error = %e, "retry budget exhausted");
                }
                return Err(e);
            }
        }
    }
    // Only reachable if max_attempts == 0 (degenerate), in which case there's
    // no value to return — bubble the last error or a stand-in. We can't
    // construct an arbitrary E here so we require the loop to always return.
    Err(last_err.expect("loop with max_attempts >= 1 must return on first failure"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    #[tokio::test]
    async fn succeeds_on_first_try() {
        let calls = Arc::new(AtomicU32::new(0));
        let calls2 = calls.clone();
        let policy = RetryPolicy {
            max_attempts: 3,
            initial_backoff: Duration::from_millis(1),
            max_backoff: Duration::from_millis(10),
            jitter: 0.0,
        };
        let result: Result<u32, &'static str> = with_backoff(
            policy,
            move || {
                let c = calls2.clone();
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    Ok::<u32, &'static str>(42)
                }
            },
            |_| true,
        )
        .await;
        assert_eq!(result.unwrap(), 42);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn retries_then_succeeds() {
        let calls = Arc::new(AtomicU32::new(0));
        let calls2 = calls.clone();
        let policy = RetryPolicy {
            max_attempts: 3,
            initial_backoff: Duration::from_millis(1),
            max_backoff: Duration::from_millis(10),
            jitter: 0.0,
        };
        let result: Result<u32, &'static str> = with_backoff(
            policy,
            move || {
                let c = calls2.clone();
                async move {
                    let n = c.fetch_add(1, Ordering::SeqCst);
                    if n < 2 {
                        Err("transient")
                    } else {
                        Ok(99)
                    }
                }
            },
            |_| true,
        )
        .await;
        assert_eq!(result.unwrap(), 99);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn bubbles_non_retryable() {
        let calls = Arc::new(AtomicU32::new(0));
        let calls2 = calls.clone();
        let policy = RetryPolicy {
            max_attempts: 3,
            initial_backoff: Duration::from_millis(1),
            max_backoff: Duration::from_millis(10),
            jitter: 0.0,
        };
        let result: Result<u32, &'static str> = with_backoff(
            policy,
            move || {
                let c = calls2.clone();
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    Err::<u32, &'static str>("permanent")
                }
            },
            |e| *e == "transient", // never retry "permanent"
        )
        .await;
        assert!(result.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1, "must not retry on non-retryable err");
    }

    #[tokio::test]
    async fn exhausts_budget_then_bubbles() {
        let calls = Arc::new(AtomicU32::new(0));
        let calls2 = calls.clone();
        let policy = RetryPolicy {
            max_attempts: 3,
            initial_backoff: Duration::from_millis(1),
            max_backoff: Duration::from_millis(10),
            jitter: 0.0,
        };
        let result: Result<u32, &'static str> = with_backoff(
            policy,
            move || {
                let c = calls2.clone();
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    Err::<u32, &'static str>("transient")
                }
            },
            |_| true,
        )
        .await;
        assert!(result.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 3, "must try exactly max_attempts times");
    }

    #[test]
    fn backoff_schedule_is_exponential_until_cap() {
        let policy = RetryPolicy {
            max_attempts: 10,
            initial_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(30),
            jitter: 0.0,
        };
        assert_eq!(policy.backoff_for(1), Duration::from_secs(1));
        assert_eq!(policy.backoff_for(2), Duration::from_secs(2));
        assert_eq!(policy.backoff_for(3), Duration::from_secs(4));
        assert_eq!(policy.backoff_for(4), Duration::from_secs(8));
        assert_eq!(policy.backoff_for(5), Duration::from_secs(16));
        assert_eq!(policy.backoff_for(6), Duration::from_secs(30)); // capped
        assert_eq!(policy.backoff_for(10), Duration::from_secs(30)); // capped
    }
}
