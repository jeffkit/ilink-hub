//! Simple in-memory per-key rate limiter.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug)]
pub struct RateLimiter {
    inner: Mutex<Inner>,
    max_events: usize,
    window_ms: i64,
}

#[derive(Debug, Default)]
struct Inner {
    buckets: HashMap<String, Vec<i64>>,
}

impl RateLimiter {
    pub fn new(max_events: usize, window_secs: u64) -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            max_events,
            window_ms: window_secs as i64 * 1000,
        }
    }

    pub fn allow(&self, key: &str) -> bool {
        let now = now_ms();
        let mut inner = self.inner.lock().expect("rate limiter lock");
        let bucket = inner.buckets.entry(key.to_string()).or_default();
        bucket.retain(|t| now - *t < self.window_ms);
        if bucket.len() >= self.max_events {
            return false;
        }
        bucket.push(now);
        // Evict keys that have had no events in the last window to bound memory.
        if inner.buckets.len() > 10_000 {
            inner.buckets.retain(|_, v| !v.is_empty());
        }
        true
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_millis() as i64
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
