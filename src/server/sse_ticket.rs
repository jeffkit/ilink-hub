//! Short-lived, single-use tickets for authenticating SSE (`EventSource`) streams.
//!
//! Browsers cannot attach an `Authorization` header to an `EventSource`, so the
//! only way to authenticate a streaming GET from the admin UI is via the URL.
//! Putting the long-lived `ILINK_ADMIN_TOKEN` directly in `?token=` leaks a
//! durable bearer credential into proxy access logs, browser history, and the
//! `Referer` header.
//!
//! Instead, the admin UI first POSTs to a normal (header-authenticated) endpoint
//! to mint a ticket, then opens the stream with `?ticket=<ticket>`. Tickets are:
//!   - high entropy (128 bits from the OS CSPRNG),
//!   - single-use (consumed on first validation), and
//!   - short-lived (expire after [`TICKET_TTL`]).
//!
//! A leaked ticket is therefore worthless seconds later and cannot be replayed.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long a freshly issued ticket remains valid. The admin UI redeems it
/// immediately, so a few seconds is plenty; keep a small margin for slow links.
pub const TICKET_TTL: Duration = Duration::from_secs(30);

/// In-memory store of outstanding SSE tickets (`ticket -> expiry instant`).
///
/// Backed by a `std::sync::Mutex`: every operation is O(map size) and never
/// awaits, so blocking the executor briefly is fine and a poisoned lock is
/// recovered rather than propagated.
#[derive(Default)]
pub struct SseTicketStore {
    inner: Mutex<HashMap<String, Instant>>,
}

impl SseTicketStore {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// Mint a new single-use ticket valid for [`TICKET_TTL`]. Expired tickets are
    /// pruned opportunistically so the map cannot grow without bound.
    pub fn issue(&self) -> String {
        use rand::RngCore;
        let mut bytes = [0u8; 16];
        rand::rng().fill_bytes(&mut bytes);
        let ticket: String = bytes.iter().map(|b| format!("{b:02x}")).collect();

        let now = Instant::now();
        let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        map.retain(|_, exp| *exp > now);
        map.insert(ticket.clone(), now + TICKET_TTL);
        ticket
    }

    /// Validate and consume a ticket. Returns `true` exactly once for a valid,
    /// unexpired ticket; every subsequent call (or an unknown/expired ticket)
    /// returns `false`.
    pub fn consume(&self, ticket: &str) -> bool {
        if ticket.is_empty() {
            return false;
        }
        let now = Instant::now();
        let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        match map.remove(ticket) {
            Some(exp) => exp > now,
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issued_ticket_can_be_consumed_once() {
        let store = SseTicketStore::new();
        let t = store.issue();
        assert!(store.consume(&t), "first consume should succeed");
        assert!(!store.consume(&t), "second consume must fail (single-use)");
    }

    #[test]
    fn unknown_or_empty_ticket_rejected() {
        let store = SseTicketStore::new();
        assert!(!store.consume(""));
        assert!(!store.consume("deadbeef"));
    }

    #[test]
    fn expired_ticket_rejected() {
        let store = SseTicketStore::new();
        let t = store.issue();
        // Force expiry by rewriting the stored instant into the past.
        {
            let mut map = store.inner.lock().unwrap();
            let exp = map.get_mut(&t).unwrap();
            *exp = Instant::now() - Duration::from_secs(1);
        }
        assert!(!store.consume(&t), "expired ticket must be rejected");
    }

    #[test]
    fn tickets_are_distinct_and_high_entropy() {
        let store = SseTicketStore::new();
        let a = store.issue();
        let b = store.issue();
        assert_ne!(a, b);
        assert_eq!(a.len(), 32, "16 bytes -> 32 hex chars");
    }
}
