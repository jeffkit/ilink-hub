//! Hub core: shared state, the inbound message dispatcher, and hub-command
//! handling.
//!
//! The module is split into cohesive submodules:
//!
//! - [`state`] — [`HubState`] and its `IlinkConnState` / `RoutingState` /
//!   `ClientState` sub-states, metrics, and long-poll tracking.
//! - [`dispatch`] — the broadcast→backend pipeline, quote resolution,
//!   `@mention` routing, and the per-conversation `HubExt` helpers.
//! - [`commands`] — the `/list`, `/use`, `/status`, `/help`, `/session …`
//!   command handlers.
//!
//! The remaining `pub mod`s (`router`, `queue`, `registry`, …) hold the routing
//! primitives and persistence-adjacent types the core orchestrates.

pub mod health;
pub mod messages;
pub mod outbound_label;
pub mod pairing;
pub mod queue;
pub mod quote_route;
pub mod registry;
pub mod router;

mod commands;
mod dispatch;
mod state;

/// iLink upstream connection status codes stored in `HubState::ilink_status`.
pub mod ilink_status {
    pub const UNKNOWN: u8 = 0;
    pub const CONNECTED: u8 = 1;
    pub const NEEDS_LOGIN: u8 = 2;
    pub const LOGGING_IN: u8 = 3;

    /// Canonical string form of a status code for API responses and log output.
    /// All known codes are listed explicitly so adding a new constant without
    /// updating this function causes a test failure (see `ilink_status_str_covers_all_codes`).
    pub fn as_str(code: u8) -> &'static str {
        match code {
            UNKNOWN => "unknown",
            CONNECTED => "connected",
            NEEDS_LOGIN => "needs_login",
            LOGGING_IN => "logging_in",
            _ => "unknown",
        }
    }
}

pub use dispatch::{spawn_dispatcher, spawn_quote_index_evictor};
pub use health::spawn_health_checker;
pub use outbound_label::{
    append_outbound_origin_footer_to_first_text_item, format_outbound_origin_line,
    should_append_outbound_origin_label,
};
pub use pairing::PairingRegistry;
pub use queue::{InMemoryQueue, MessageQueue};
pub use quote_route::{merge_routing_with_quote, QuoteOrigin, QuoteRouteIndex};
pub use registry::{ClientInfo, ClientRegistry};
pub use router::{HubCommand, Router, RoutingDecision};
pub use state::{
    AdminConfig, ClientState, EnterOutcome, HubState, IlinkConnState, LatencyGuard,
    LatencyHistogram, Metrics, PollGuard, PollTracker, RoutingState, HISTOGRAM_BUCKETS_MS,
    MAX_CONCURRENT_POLLS_PER_VTOKEN, MAX_HUB_POLLS_DEFAULT,
};


#[cfg(test)]
mod tests;

#[cfg(test)]
mod ilink_status_tests {
    use super::ilink_status;

    /// Ensure every defined constant maps to a non-"unknown" string.
    /// If a new constant is added without updating `as_str`, this test catches it.
    #[test]
    fn ilink_status_str_covers_all_codes() {
        let known = [
            (ilink_status::UNKNOWN, "unknown"),
            (ilink_status::CONNECTED, "connected"),
            (ilink_status::NEEDS_LOGIN, "needs_login"),
            (ilink_status::LOGGING_IN, "logging_in"),
        ];
        for (code, expected) in known {
            assert_eq!(
                ilink_status::as_str(code),
                expected,
                "as_str({code}) should return \"{expected}\""
            );
        }
        // Unknown code falls back to "unknown" rather than panicking.
        assert_eq!(ilink_status::as_str(99), "unknown");
    }
}
