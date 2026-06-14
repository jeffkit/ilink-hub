//! Simple in-memory per-key rate limiter.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

#[derive(Debug)]
pub struct RateLimiter {
    inner: Mutex<Inner>,
    max_events: usize,
    window_ms: u64,
}

#[derive(Debug, Default)]
struct Inner {
    buckets: HashMap<String, Vec<Instant>>,
}

impl RateLimiter {
    pub fn new(max_events: usize, window_secs: u64) -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            max_events,
            window_ms: window_secs * 1000,
        }
    }

    pub fn allow(&self, key: &str) -> bool {
        let now = Instant::now();
        let window = std::time::Duration::from_millis(self.window_ms);
        let mut inner = self.inner.lock().expect("rate limiter lock");
        let bucket = inner.buckets.entry(key.to_string()).or_default();
        bucket.retain(|t| now.duration_since(*t) < window);
        if bucket.len() >= self.max_events {
            return false;
        }
        bucket.push(now);
        // Evict stale keys to bound memory growth. Apply the window filter to
        // all buckets so entries with only expired timestamps are removed, not
        // just buckets that are currently empty.
        if inner.buckets.len() > 10_000 {
            inner.buckets.retain(|_, v| {
                v.retain(|t| now.duration_since(*t) < window);
                !v.is_empty()
            });
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
}
