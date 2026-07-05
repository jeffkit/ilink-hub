//! Simple in-memory per-key rate limiter using a fixed-window counter.
//!
//! Each bucket tracks `(count, window_start)`. On every request the window is
//! reset when it has expired, then the counter is incremented. This is O(1)
//! per `allow()` call instead of the O(N) `Vec<Instant>::retain` approach.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Debug)]
pub struct RateLimiter {
    inner: Mutex<Inner>,
    max_events: usize,
    window: Duration,
}

#[derive(Debug, Default)]
struct Inner {
    buckets: HashMap<String, Bucket>,
}

#[derive(Debug)]
struct Bucket {
    count: usize,
    window_start: Instant,
}

impl RateLimiter {
    pub fn new(max_events: usize, window_secs: u64) -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            max_events,
            window: Duration::from_secs(window_secs),
        }
    }

    pub fn allow(&self, key: &str) -> bool {
        let now = Instant::now();
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let bucket = inner
            .buckets
            .entry(key.to_string())
            .or_insert_with(|| Bucket {
                count: 0,
                window_start: now,
            });

        if now.duration_since(bucket.window_start) >= self.window {
            bucket.count = 0;
            bucket.window_start = now;
        }

        if bucket.count >= self.max_events {
            return false;
        }
        bucket.count += 1;

        // Evict stale keys to bound memory growth.
        if inner.buckets.len() > 10_000 {
            inner
                .buckets
                .retain(|_, b| now.duration_since(b.window_start) < self.window);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_after_max() {
        let limiter = RateLimiter::new(2, 60);
        assert!(limiter.allow("1.2.3.4"));
        assert!(limiter.allow("1.2.3.4"));
        assert!(!limiter.allow("1.2.3.4"));
        assert!(limiter.allow("5.6.7.8"));
    }

    #[test]
    fn resets_after_window() {
        let limiter = RateLimiter::new(1, 0);
        assert!(limiter.allow("a"));
        // window_secs=0 means the window is 0 Duration; any subsequent call
        // that observes elapsed >= window resets the counter.
        assert!(limiter.allow("a"));
    }

    #[test]
    fn test_ratelimit_poison_safe() {
        use std::sync::Arc;
        use std::thread;

        let limiter = Arc::new(RateLimiter::new(2, 60));
        let limiter_clone = Arc::clone(&limiter);

        // Explicitly lock and panic in a sub-thread to poison the Mutex
        let handle = thread::spawn(move || {
            let _lock = limiter_clone.inner.lock().unwrap();
            panic!("poisoning the lock");
        });
        let _ = handle.join();

        // When the Mutex is poisoned, calling allow should still succeed and not panic
        assert!(limiter.allow("1.2.3.4"));
        assert!(limiter.allow("1.2.3.4"));
        assert!(!limiter.allow("1.2.3.4"));
    }

    /// Cover the eviction branch: inserting > 10_000 distinct keys triggers
    /// `buckets.retain(...)` which prunes stale entries.
    ///
    /// Using `window_secs = 0` makes every bucket immediately stale, so the
    /// retain predicate removes all of them. Afterwards the limiter must still
    /// function correctly, proving both the `> 10_000` threshold comparison and
    /// the `< self.window` filter comparison are exercised.
    #[test]
    fn evicts_stale_keys_when_over_limit() {
        // window = 0 → every bucket expires immediately after creation.
        let limiter = RateLimiter::new(1, 0);

        // Push past the 10_000-key eviction threshold.
        for i in 0..=10_000usize {
            limiter.allow(&i.to_string());
        }

        // After eviction the limiter must still accept new keys normally.
        assert!(
            limiter.allow("new_key_after_eviction"),
            "limiter must work correctly after evicting stale keys"
        );
    }

    /// M2: eviction must trigger only when len is strictly > 10_000.
    #[test]
    fn eviction_threshold_is_strict_greater_than_10000() {
        // window=0 → each bucket immediately expires
        let limiter = RateLimiter::new(1_000_000, 0);

        // Insert exactly 10_000 distinct keys
        for i in 0..10_000usize {
            limiter.allow(&i.to_string());
        }

        // Assert: 10_000 buckets exist (eviction threshold is > 10_000, not >=)
        let inner = limiter.inner.lock().unwrap();
        assert_eq!(
            inner.buckets.len(),
            10_000,
            "eviction must not trigger at exactly 10,000 buckets (threshold is strictly > 10_000)"
        );
    }

    /// M2: retain predicate must keep fresh buckets and evict stale ones.
    #[test]
    fn retain_keeps_fresh_buckets_and_evicts_stale_ones() {
        // window = 60s: buckets older than 60s are stale
        let limiter = RateLimiter::new(1_000_000, 60);
        let now = Instant::now();
        let stale_start = now - Duration::from_secs(120);

        {
            let mut inner = limiter.inner.lock().unwrap();
            for i in 0..10_000usize {
                inner.buckets.insert(
                    format!("stale_{i}"),
                    Bucket {
                        count: 1,
                        window_start: stale_start,
                    },
                );
            }
        }

        // allow("fresh_key"): triggers eviction (10_001 > 10_000)
        limiter.allow("fresh_key");

        let inner = limiter.inner.lock().unwrap();
        assert_eq!(
            inner.buckets.len(),
            1,
            "after eviction, only the fresh key must remain"
        );
        assert!(
            inner.buckets.contains_key("fresh_key"),
            "the fresh key (within window) must be retained after eviction"
        );
    }
}
