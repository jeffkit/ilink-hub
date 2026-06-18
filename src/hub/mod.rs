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
    ClientState, HubState, IlinkConnState, Metrics, PollGuard, PollTracker, RoutingState,
    MAX_CONCURRENT_POLLS_PER_VTOKEN,
};

pub use ilink_status as IlinkStatus;

#[cfg(test)]
mod tests;
