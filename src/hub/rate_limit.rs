//! Per-vtoken token-bucket rate limiting for the outbound bot API surface.
//!
//! Every tenant of the Hub shares one real WeChat account and one upstream
//! connection pool, so the upstream quota is global even though the virtual
//! tokens are per-tenant. An unbounded tenant is therefore a noisy neighbour
//! for the whole fleet.
//!
//! Before this module existed, the outbound surface was protected only in
//! dimensions that do not isolate tenants:
//!
//! - `getupdates` — per-vtoken + Hub-wide *concurrency* caps
//!   ([`super::PollTracker`]). Not a rate cap, and it does not cover writes.
//! - `sendmessage` — one Hub-wide `ConcurrencyLimitLayer` (64). A single
//!   tenant can occupy all 64 slots.
//! - `sendtyping` / `getconfig` / `getuploadurl` — no gate at all.
//!
//! This module adds the missing per-tenant dimension: one token bucket per
//! vtoken, shared by all four outbound bot routes and by the MCP `call_agent`
//! tool, so a tenant hammering `sendtyping` cannot starve another tenant's
//! `sendmessage`.
//!
//! Buckets are only ever created for vtokens that already passed the registry
//! check, so the map is bounded by the number of registered clients;
//! [`VtokenRateLimiter::capacity`] is a defensive second bound.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex as StdMutex;
use std::time::{Duration, Instant};

/// Default sustained refill rate per vtoken, in requests per second.
///
/// Deliberately generous: a healthy bridge talking to one WeChat conversation
/// stays well under 1 req/s, and the limit exists to stop runaway retry loops
/// and noisy neighbours, not to shape normal traffic. Raise it via
/// `ILINK_BOT_RATE_LIMIT_PER_SEC` if a legitimate workload is ever throttled.
pub const BOT_RATE_LIMIT_PER_SEC_DEFAULT: f64 = 20.0;

/// Default burst capacity per vtoken, in requests.
///
/// Two seconds' worth of the default rate, so short legitimate bursts (a
/// multi-part reply, a reconnect that re-sends typing state) pass without
/// being shaped, while a sustained flood is still cut off.
pub const BOT_RATE_LIMIT_BURST_DEFAULT: f64 = 40.0;

/// Hard cap on tracked buckets. Registered vtokens are already bounded by the
/// client registry; this is a defensive bound so a bug that feeds unregistered
/// tokens into [`VtokenRateLimiter::check`] cannot grow the map without limit.
pub const BOT_RATE_LIMIT_MAX_ENTRIES: usize = 4096;

/// Outcome of a single [`VtokenRateLimiter::check`] call.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RateLimitOutcome {
    /// Request admitted. `remaining` is the token count left in the bucket
    /// after the deduction (always `< burst`).
    Allowed { remaining: f64 },
    /// Request rejected. `retry_after` is how long the caller must wait for
    /// the bucket to hold one whole token again; it is never zero.
    Denied { retry_after: Duration },
}

/// Clamp a configured rate to a usable value. A zero/negative/NaN rate would
/// either divide by zero on the reject path or turn the limiter into an
/// infinite black hole; `0.001` tokens/s degrades to "strict but alive".
fn sanitize_rate(rate_per_sec: f64) -> f64 {
    if rate_per_sec.is_finite() && rate_per_sec > 0.0 {
        rate_per_sec
    } else {
        0.001
    }
}

/// Clamp a configured burst to at least one token, so a client can always
/// eventually get a request through.
fn sanitize_burst(burst: f64) -> f64 {
    if burst.is_finite() && burst >= 1.0 {
        burst
    } else {
        1.0
    }
}

/// One token bucket plus the per-tenant rejection counter, so the metrics
/// endpoint can report "how much quota is left" and "how often was this tenant
/// throttled" from a single snapshot.
#[derive(Debug, Clone, Copy)]
struct Bucket {
    /// Whole tokens currently available, in `[0, burst]`.
    tokens: f64,
    /// Last time the bucket was refilled (or created).
    last: Instant,
    /// Requests rejected for this vtoken since process start.
    rejected: u64,
}

/// Point-in-time view of one tenant's bucket, for the metrics endpoint.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RateLimitSnapshot {
    /// Tokens available right now.
    pub tokens: f64,
    /// Configured burst capacity (the maximum `tokens` can reach).
    pub burst: f64,
    /// Requests rejected for this vtoken since process start.
    pub rejected: u64,
}

/// Per-vtoken token-bucket limiter. Cheap enough to call on every outbound
/// request: one `std::sync::Mutex` acquire, no allocation on the hit path
/// (the per-vtoken key already exists after the first call).
#[derive(Debug)]
pub struct VtokenRateLimiter {
    /// vtoken hash → bucket. Private: everything outside this module goes
    /// through [`VtokenRateLimiter::check`] / [`VtokenRateLimiter::snapshot`],
    /// which keep the poison handling in one place. The module's own tests
    /// reach in directly to poison the lock on purpose.
    buckets: StdMutex<HashMap<String, Bucket>>,
    /// Sustained refill rate, tokens per second, as `f64::to_bits`.
    ///
    /// An atomic rather than a plain `f64` so operators can retune the policy
    /// at startup via [`VtokenRateLimiter::set_limits`] after the limiter has
    /// already been constructed by `ClientState::new` — the same
    /// set-once-before-serving pattern [`super::PollTracker::set_hub_cap`]
    /// uses. Retuning *while* traffic is in flight is not atomic across the
    /// two fields, but each individual read is consistent, and the only
    /// caller runs before the listener accepts.
    rate_per_sec_bits: AtomicU64,
    /// Burst capacity, tokens, as `f64::to_bits`. See `rate_per_sec_bits`.
    burst_bits: AtomicU64,
    /// Defensive cap on `buckets.len()`; see [`BOT_RATE_LIMIT_MAX_ENTRIES`].
    capacity: usize,
}

impl Default for VtokenRateLimiter {
    fn default() -> Self {
        Self::new(BOT_RATE_LIMIT_PER_SEC_DEFAULT, BOT_RATE_LIMIT_BURST_DEFAULT)
    }
}

impl VtokenRateLimiter {
    /// Build a limiter with an explicit rate/burst. Zero or negative rates are
    /// clamped to a tiny positive value rather than rejected: a misconfigured
    /// `ILINK_BOT_RATE_LIMIT_PER_SEC=0` should degrade to "very strict", not
    /// panic at startup or divide by zero on the reject path.
    pub fn new(rate_per_sec: f64, burst: f64) -> Self {
        Self {
            buckets: StdMutex::new(HashMap::new()),
            rate_per_sec_bits: AtomicU64::new(sanitize_rate(rate_per_sec).to_bits()),
            burst_bits: AtomicU64::new(sanitize_burst(burst).to_bits()),
            capacity: BOT_RATE_LIMIT_MAX_ENTRIES,
        }
    }

    /// Retune the policy. Intended to be called once at startup, after
    /// construction and before the listener accepts traffic; see the note on
    /// `rate_per_sec_bits`.
    pub fn set_limits(&self, rate_per_sec: f64, burst: f64) {
        self.rate_per_sec_bits
            .store(sanitize_rate(rate_per_sec).to_bits(), Ordering::Relaxed);
        self.burst_bits
            .store(sanitize_burst(burst).to_bits(), Ordering::Relaxed);
    }

    /// Configured sustained rate, tokens per second. For startup logging.
    pub fn rate_per_sec(&self) -> f64 {
        f64::from_bits(self.rate_per_sec_bits.load(Ordering::Relaxed))
    }

    /// Configured burst capacity, tokens. For startup logging and metric HELP.
    pub fn burst(&self) -> f64 {
        f64::from_bits(self.burst_bits.load(Ordering::Relaxed))
    }

    /// Admit or reject one outbound request for `vtoken`.
    pub fn check(&self, vtoken: &str) -> RateLimitOutcome {
        self.check_at(vtoken, Instant::now())
    }

    /// [`VtokenRateLimiter::check`] with an explicit clock. Tests drive time
    /// through this so refill behaviour is deterministic instead of
    /// wall-clock dependent.
    pub fn check_at(&self, vtoken: &str, now: Instant) -> RateLimitOutcome {
        let Ok(mut buckets) = self.buckets.lock() else {
            // Fail open. A poisoned bucket map is a process-wide bug, but
            // turning it into "reject every outbound message forever" would
            // take the whole Hub down — the same trade-off `PollTracker`
            // makes on its poisoned per-vtoken path. The poison itself is
            // already visible as the panic that caused it.
            return RateLimitOutcome::Allowed {
                remaining: self.burst(),
            };
        };

        // Bound the map. Only registered vtokens reach this point, so the cap
        // is a backstop rather than the primary bound; evict the
        // least-recently-used bucket to make room.
        if buckets.len() >= self.capacity && !buckets.contains_key(vtoken) {
            if let Some(oldest) = buckets
                .iter()
                .min_by_key(|(_, b)| b.last)
                .map(|(k, _)| k.clone())
            {
                buckets.remove(&oldest);
            }
        }

        // Read the policy once per call so a concurrent retune cannot produce a
        // mixed rate/burst pair within a single refill calculation.
        let burst = self.burst();
        let rate = self.rate_per_sec();
        let bucket = buckets.entry(vtoken.to_string()).or_insert(Bucket {
            tokens: burst,
            last: now,
            rejected: 0,
        });

        // `saturating_duration_since` rather than `duration_since`: a caller
        // that passes a `now` older than the bucket's `last` must not be
        // credited tokens (nor panic in debug builds).
        let elapsed = now.saturating_duration_since(bucket.last).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * rate).min(burst);
        bucket.last = now;

        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            RateLimitOutcome::Allowed {
                remaining: bucket.tokens,
            }
        } else {
            bucket.rejected = bucket.rejected.saturating_add(1);
            // Time for the deficit to refill into one whole token. `rate` is
            // clamped positive in `new`, so this cannot divide by zero.
            RateLimitOutcome::Denied {
                retry_after: Duration::from_secs_f64((1.0 - bucket.tokens) / rate),
            }
        }
    }

    /// Snapshot every tracked bucket: `(vtoken hash, snapshot)`. Called only
    /// by the metrics endpoint (once per scrape), never on the request path.
    pub fn snapshot(&self) -> Vec<(String, RateLimitSnapshot)> {
        let Ok(buckets) = self.buckets.lock() else {
            return Vec::new();
        };
        buckets
            .iter()
            .map(|(k, b)| {
                (
                    k.clone(),
                    RateLimitSnapshot {
                        tokens: b.tokens,
                        burst: self.burst(),
                        rejected: b.rejected,
                    },
                )
            })
            .collect()
    }

    /// Test-only: number of tracked buckets, used by the bounded-map test.
    #[doc(hidden)]
    pub fn tracked_count(&self) -> usize {
        self.buckets
            .lock()
            .map(|b| b.len())
            .unwrap_or_else(|e| e.into_inner().len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn limiter(rate: f64, burst: f64) -> VtokenRateLimiter {
        VtokenRateLimiter::new(rate, burst)
    }

    #[test]
    fn allows_up_to_burst_then_denies() {
        let l = limiter(1.0, 3.0);
        let t0 = Instant::now();
        for i in 0..3 {
            assert!(
                matches!(l.check_at("vt", t0), RateLimitOutcome::Allowed { .. }),
                "request {i} within burst must be admitted"
            );
        }
        match l.check_at("vt", t0) {
            RateLimitOutcome::Denied { retry_after } => {
                // One whole token missing at 1 token/s → ~1s.
                assert!(
                    retry_after >= Duration::from_millis(900),
                    "retry_after should be ~1s, got {retry_after:?}"
                );
            }
            other => panic!("4th request must be denied, got {other:?}"),
        }
    }

    #[test]
    fn refills_over_time() {
        let l = limiter(2.0, 2.0);
        let t0 = Instant::now();
        assert!(matches!(
            l.check_at("vt", t0),
            RateLimitOutcome::Allowed { .. }
        ));
        assert!(matches!(
            l.check_at("vt", t0),
            RateLimitOutcome::Allowed { .. }
        ));
        assert!(matches!(
            l.check_at("vt", t0),
            RateLimitOutcome::Denied { .. }
        ));

        // 500 ms at 2 tokens/s refills exactly one token.
        let t1 = t0 + Duration::from_millis(500);
        assert!(
            matches!(l.check_at("vt", t1), RateLimitOutcome::Allowed { .. }),
            "half a second at 2/s must refill one token"
        );
        assert!(
            matches!(l.check_at("vt", t1), RateLimitOutcome::Denied { .. }),
            "only one token was refilled"
        );
    }

    #[test]
    fn refill_is_capped_at_burst() {
        let l = limiter(10.0, 5.0);
        let t0 = Instant::now();
        // Drain, then idle for far longer than it takes to refill.
        for _ in 0..5 {
            assert!(matches!(
                l.check_at("vt", t0),
                RateLimitOutcome::Allowed { .. }
            ));
        }
        let t1 = t0 + Duration::from_secs(3600);
        let mut allowed = 0;
        for _ in 0..100 {
            if matches!(l.check_at("vt", t1), RateLimitOutcome::Allowed { .. }) {
                allowed += 1;
            }
        }
        assert_eq!(
            allowed, 5,
            "an hour of idling must not exceed the burst cap"
        );
    }

    #[test]
    fn buckets_are_per_vtoken() {
        let l = limiter(1.0, 1.0);
        let t0 = Instant::now();
        assert!(matches!(
            l.check_at("vt-a", t0),
            RateLimitOutcome::Allowed { .. }
        ));
        assert!(
            matches!(l.check_at("vt-a", t0), RateLimitOutcome::Denied { .. }),
            "vt-a is drained"
        );
        assert!(
            matches!(l.check_at("vt-b", t0), RateLimitOutcome::Allowed { .. }),
            "vt-b must not inherit vt-a's exhaustion (noisy-neighbour isolation)"
        );
    }

    #[test]
    fn denied_attempts_are_counted_per_vtoken() {
        let l = limiter(1.0, 1.0);
        let t0 = Instant::now();
        let _ = l.check_at("vt", t0); // allowed
        let _ = l.check_at("vt", t0); // denied
        let _ = l.check_at("vt", t0); // denied

        let snapshot = l.snapshot();
        let (key, view) = snapshot
            .iter()
            .find(|(k, _)| k == "vt")
            .expect("vt must be tracked");
        assert_eq!(key, "vt");
        assert_eq!(view.rejected, 2);
        assert_eq!(view.burst, 1.0);
        assert!(view.tokens < 1.0);
    }

    #[test]
    fn bucket_map_is_bounded() {
        let mut l = limiter(1.0, 1.0);
        l.capacity = 8;
        let t0 = Instant::now();
        for i in 0..64 {
            // Advance the clock so eviction has a well-defined "oldest".
            let _ = l.check_at(&format!("vt-{i}"), t0 + Duration::from_millis(i));
        }
        assert!(
            l.tracked_count() <= 8,
            "bucket map must stay at or below capacity, got {}",
            l.tracked_count()
        );
    }

    #[test]
    fn eviction_keeps_the_most_recent_bucket() {
        let mut l = limiter(1.0, 1.0);
        l.capacity = 2;
        let t0 = Instant::now();
        let _ = l.check_at("old", t0);
        let _ = l.check_at("mid", t0 + Duration::from_millis(1));
        // Third key evicts "old" (oldest `last`), not "mid".
        let _ = l.check_at("new", t0 + Duration::from_millis(2));
        assert_eq!(l.tracked_count(), 2);
        let keys: Vec<String> = l.snapshot().into_iter().map(|(k, _)| k).collect();
        assert!(
            !keys.contains(&"old".to_string()),
            "oldest bucket should be evicted, got {keys:?}"
        );
    }

    #[test]
    fn nonpositive_rate_is_clamped_not_panicking() {
        // Operators can set ILINK_BOT_RATE_LIMIT_PER_SEC=0 by mistake; the
        // limiter must degrade to near-zero throughput, not panic or hang.
        let l = VtokenRateLimiter::new(0.0, 0.0);
        assert!(l.rate_per_sec() > 0.0);
        assert!(l.burst() >= 1.0);
        let t0 = Instant::now();
        assert!(matches!(
            l.check_at("vt", t0),
            RateLimitOutcome::Allowed { .. }
        ));
        assert!(matches!(
            l.check_at("vt", t0),
            RateLimitOutcome::Denied { .. }
        ));
    }

    /// The noisy-neighbour property this whole issue is about: with the bucket
    /// drained, a concurrent flood from many tasks must admit exactly the
    /// remaining tokens — no more, regardless of interleaving.
    #[test]
    fn concurrent_flood_admits_exactly_the_available_tokens() {
        const THREADS: usize = 8;
        const PER_THREAD: usize = 100;
        // Refill rate is negligible here; the point is the burst accounting.
        let l = Arc::new(limiter(1e-9, 16.0));
        // Freeze time: every call uses the same instant, so no refill can
        // occur and the admitted count must be exactly the burst.
        let t0 = Instant::now();

        let allowed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut handles = Vec::new();
        for _ in 0..THREADS {
            let l = Arc::clone(&l);
            let allowed = Arc::clone(&allowed);
            handles.push(std::thread::spawn(move || {
                for _ in 0..PER_THREAD {
                    if matches!(l.check_at("shared", t0), RateLimitOutcome::Allowed { .. }) {
                        allowed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                }
            }));
        }
        for h in handles {
            h.join().expect("worker thread must not panic");
        }

        assert_eq!(
            allowed.load(std::sync::atomic::Ordering::Relaxed),
            16,
            "exactly the burst may be admitted under concurrent load"
        );
        let rejected = l
            .snapshot()
            .into_iter()
            .find(|(k, _)| k == "shared")
            .map(|(_, v)| v.rejected)
            .unwrap_or(0);
        assert_eq!(
            rejected as usize,
            THREADS * PER_THREAD - 16,
            "every non-admitted call must be counted as a rejection"
        );
    }

    #[test]
    fn poisoned_mutex_fails_open() {
        let l = Arc::new(limiter(1.0, 1.0));
        let l2 = Arc::clone(&l);
        let _ = std::thread::spawn(move || {
            let _guard = l2.buckets.lock().unwrap();
            panic!("poison the bucket mutex on purpose");
        })
        .join();

        // Fail open: a poisoned map must not turn into a Hub-wide outage.
        assert!(
            matches!(l.check("vt"), RateLimitOutcome::Allowed { .. }),
            "poisoned limiter must fail open"
        );
        assert!(l.snapshot().is_empty());
    }
}
