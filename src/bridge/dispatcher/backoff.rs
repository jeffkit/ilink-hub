use std::time::Duration;

/// Initial backoff after the first throttled (`ret == -2`) response. Doubles
/// on every consecutive throttle up to [`MAX_BACKOFF_SECS`].
pub(super) const INITIAL_BACKOFF_SECS: u64 = 5;
/// Hard cap on the backoff between throttle retries. Once an attempt count
/// would push the wait past this value, the loop holds at this interval
/// until either the send lands or the cumulative retry budget (M4,
/// [`retry_budget`]) is exhausted.
pub(super) const MAX_BACKOFF_SECS: u64 = 60;

/// Floor / ceiling for the M4 cumulative retry budget. A single buffered
/// chunk (or one final reply) is retried under persistent throttling for at
/// most this long before we give up, log an `error!`, and move on. We tie
/// the budget to the CLI `timeout_secs` so a long-running task earns a
/// proportionally long delivery window, but clamp it so a tiny or huge
/// timeout still yields a sane window (the upper bound roughly matches the
/// observed WeChat ~5-7 min throttle cooldown).
pub(super) const MIN_RETRY_BUDGET_SECS: u64 = 60;
pub(super) const MAX_RETRY_BUDGET_SECS: u64 = 300;

/// Cumulative wall-clock budget for retrying a throttled send (M4).
///
/// Derived from the profile's `timeout_secs`, clamped to
/// `[MIN_RETRY_BUDGET_SECS, MAX_RETRY_BUDGET_SECS]`. Pure so it can be
/// unit-pinned.
pub(super) fn retry_budget(profile_timeout_secs: u64) -> Duration {
    Duration::from_secs(profile_timeout_secs.clamp(MIN_RETRY_BUDGET_SECS, MAX_RETRY_BUDGET_SECS))
}

/// Pure backoff schedule for throttled `sendmessage` retries.
///
/// `attempt` is 0-based: `attempt == 0` is the **first** retry after a
/// throttle, so the first returned value is `INITIAL_BACKOFF_SECS`. The
/// sequence is therefore `5s, 10s, 20s, 40s, 60s, 60s, …`. Saturates at
/// [`MAX_BACKOFF_SECS`] for any `attempt` large enough to overflow or
/// exceed the cap.
pub(super) fn backoff_for(attempt: u32) -> Duration {
    backoff_for_with(attempt, INITIAL_BACKOFF_SECS, MAX_BACKOFF_SECS)
}

/// Internal helper exposed for testing — lets the test inject a smaller
/// cap so it doesn't have to sleep tens of seconds to observe multiple
/// retries. Operates in **milliseconds** so tests can use sub-second
/// schedules (e.g. 5ms initial, 40ms cap) without losing the exponential
/// shape via `as_secs()` truncation. F-M2-001.
#[cfg(test)]
pub(super) fn backoff_for_test(attempt: u32, initial: Duration, cap: Duration) -> Duration {
    backoff_for_with_millis(
        attempt,
        initial.as_millis().max(1) as u64,
        cap.as_millis().max(1) as u64,
    )
}

fn backoff_for_with(attempt: u32, initial_secs: u64, max_secs: u64) -> Duration {
    backoff_for_with_millis(
        attempt,
        initial_secs.saturating_mul(1000),
        max_secs.saturating_mul(1000),
    )
}

fn backoff_for_with_millis(attempt: u32, initial_ms: u64, max_ms: u64) -> Duration {
    // attempt 0 -> initial_ms, attempt 1 -> 2*initial_ms, ...
    // Multiply by 2^attempt, then clamp. Avoid u64 overflow by bounding
    // the shift to a value well past the cap.
    const SATURATION_SHIFT: u32 = 20; // 2^20 * initial ≈ far past any practical cap.
    let shift = attempt.min(SATURATION_SHIFT);
    let multiplier = 1_u64.checked_shl(shift).unwrap_or(u64::MAX);
    let raw = initial_ms.saturating_mul(multiplier);
    Duration::from_millis(raw.min(max_ms))
}
