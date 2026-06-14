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
        let mut inner = self.inner.lock().expect("rate limiter lock");
        let bucket = inner.buckets.entry(key.to_string()).or_insert_with(|| Bucket {
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
}
